//! First-class remote agents (docs/design/07-remote-agents.md): single-use invites that admit
//! an agent with a scoped token, presence, ephemeral expiry, and kicking.

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{Error, Result};
use crate::identity::{register_tx, resolve_agent, AgentRef, Program, RegisterAgent};
use crate::lanes::resolve_lane;
use crate::store::{new_id, Store, Tx};
use crate::time::fmt_ts;
use crate::tokens::{create_token_tx, glob_allows, random_hex, TokenScope};

const CODE_PREFIX: &str = "anj_";
/// Seen within this long: `online`.
pub const ONLINE_WINDOW_MS: i64 = 5 * 60 * 1000;
pub const DEFAULT_INVITE_SECONDS: i64 = 24 * 3600;
pub const MAX_INVITE_SECONDS: i64 = 30 * 24 * 3600;
/// Default of the `remote_idle_hours` node setting.
pub const DEFAULT_IDLE_HOURS: u32 = 4;
const INVALID: &str = "invalid or expired invite";

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct AgentInvite {
    pub invite_id: String,
    pub agent: String,
    pub program: String,
    pub project_globs: Vec<String>,
    pub lane: Option<String>,
    pub uses: u32,
    pub max_uses: u32,
    pub ephemeral: bool,
    pub idle_hours: u32,
    pub token_ttl_seconds: Option<i64>,
    pub created_at: String,
    pub expires_at: String,
    pub revoked_at: Option<String>,
    /// `active`, `used`, `expired` or `revoked`.
    pub status: String,
}

/// Input for [`Store::create_agent_invite`].
#[derive(Debug, Clone)]
pub struct NewAgentInvite {
    pub agent: String,
    pub program: Program,
    pub project_globs: Vec<String>,
    /// Lane id or `name@project`; must be open and in a project matching the globs.
    pub lane: Option<String>,
    pub expires_in_seconds: Option<i64>,
    pub max_uses: Option<u32>,
    /// Ephemeral agents are removed when their session ends or after `idle_hours` unseen.
    pub ephemeral: bool,
    pub idle_hours: Option<u32>,
    /// Expiry of each issued token (default: none; presence cleans up ephemeral agents).
    pub token_ttl_seconds: Option<i64>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct CreatedInvite {
    pub invite: AgentInvite,
    /// The single-use code (`anj_…`). Shown once.
    pub code: String,
}

/// What a joining client says about itself (recorded, never trusted).
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct JoinClient {
    pub hostname: Option<String>,
    pub version: Option<String>,
}

/// The result of redeeming an invite.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Joined {
    pub invite_id: String,
    pub agent: String,
    pub agent_id: String,
    pub program: String,
    /// Bearer token (`ano_…`). Shown once.
    pub token: String,
    pub token_id: String,
    pub project_globs: Vec<String>,
    pub project: Option<String>,
    pub lane: Option<String>,
    pub ephemeral: bool,
    pub idle_hours: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    Online,
    Idle,
    Gone,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct RemoteAgent {
    pub agent: String,
    pub program: String,
    pub presence: Presence,
    pub last_seen_at: Option<String>,
    pub joined_at: String,
    pub invite_id: String,
    pub token_id: String,
    pub lane: Option<String>,
    pub ephemeral: bool,
    pub idle_hours: u32,
    pub client_hostname: Option<String>,
    pub left_at: Option<String>,
    pub left_reason: Option<String>,
}

/// What leaving / kicking cleaned up.
#[derive(Debug, Clone, Default, Serialize, schemars::JsonSchema)]
pub struct Departure {
    pub agent: String,
    pub reason: String,
    pub tokens_revoked: u32,
    pub holds_released: u32,
    pub leases_closed: u32,
    /// Lanes told with `lane.agent_left`.
    pub lanes: Vec<String>,
}

/// The invite id inside a code, if it parses (for the audit log).
pub fn invite_id_of(code: &str) -> Option<String> {
    let rest = code.trim().strip_prefix(CODE_PREFIX)?;
    let (id, secret) = rest.split_once('_')?;
    (id.len() == 26 && !secret.is_empty()).then(|| id.to_string())
}

fn status_of(revoked: bool, expired: bool, used_up: bool) -> &'static str {
    if revoked {
        "revoked"
    } else if used_up {
        "used"
    } else if expired {
        "expired"
    } else {
        "active"
    }
}

const INVITE_SELECT: &str = "SELECT i.invite_id, i.agent_name, i.program, i.project_globs, l.name,
       i.uses, i.max_uses, i.ephemeral,
       COALESCE(i.idle_hours, (SELECT CAST(value AS INTEGER) FROM node_settings
                               WHERE key = 'remote_idle_hours'), 4),
       i.token_ttl, i.created_at, i.expires_at, i.revoked_at, i.lane_id, i.code_hash
     FROM agent_invites i LEFT JOIN lanes l ON l.lane_id = i.lane_id";

/// A membership's idle limit in hours: its invite's `--idle-hours`, else the node's
/// `remote_idle_hours` setting (default 4), read when it is checked.
const IDLE_HOURS_SQL: &str = "COALESCE(r.idle_hours, (SELECT CAST(value AS INTEGER)
     FROM node_settings WHERE key = 'remote_idle_hours'), 4)";

struct InviteRow {
    invite: AgentInvite,
    lane_id: Option<String>,
    code_hash: String,
    expires: i64,
}

fn invite_from_row(now: i64) -> impl Fn(&Row<'_>) -> rusqlite::Result<InviteRow> {
    move |r| {
        let globs: String = r.get(3)?;
        let uses: u32 = r.get(5)?;
        let max_uses: u32 = r.get(6)?;
        let expires: i64 = r.get(11)?;
        let revoked: Option<i64> = r.get(12)?;
        Ok(InviteRow {
            invite: AgentInvite {
                invite_id: r.get(0)?,
                agent: r.get(1)?,
                program: r.get(2)?,
                project_globs: serde_json::from_str(&globs).unwrap_or_default(),
                lane: r.get(4)?,
                uses,
                max_uses,
                ephemeral: r.get(7)?,
                idle_hours: r.get(8)?,
                token_ttl_seconds: r.get(9)?,
                created_at: fmt_ts(r.get(10)?),
                expires_at: fmt_ts(expires),
                revoked_at: revoked.map(fmt_ts),
                status: status_of(revoked.is_some(), expires <= now, uses >= max_uses).into(),
            },
            lane_id: r.get(13)?,
            code_hash: r.get(14)?,
            expires,
        })
    }
}

/// Projects on this node whose path or slug matches the globs.
fn matching_projects(
    conn: &Connection,
    node_id: &str,
    globs: &[String],
) -> Result<Vec<(String, String)>> {
    let mut stmt =
        conn.prepare("SELECT project_id, abs_path, slug FROM projects WHERE node_id = ?1")?;
    let rows = stmt
        .query_map([node_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter(|(_, path, slug)| glob_allows(globs, &[path.clone(), slug.clone()]))
        .map(|(id, path, _)| (id, path))
        .collect())
}

fn agent_by_id(conn: &Connection, id: &str) -> Result<AgentRef> {
    Ok(conn.query_row(
        "SELECT agent_id, address, node_id FROM agents WHERE agent_id = ?1",
        [id],
        |r| {
            Ok(AgentRef {
                id: r.get(0)?,
                address: r.get(1)?,
                node_id: r.get(2)?,
            })
        },
    )?)
}

/// Clean up after an agent that left, went idle or was kicked. `all_tokens` revokes every
/// token of the agent (kick); otherwise only the tokens its invites issued.
///
/// With `scope` (the agent leaving through its own token), only holds and leases in the
/// token's projects are released; anything it holds elsewhere is left alone.
pub(crate) fn depart_tx(
    tx: &Tx<'_>,
    agent: &AgentRef,
    reason: &str,
    all_tokens: bool,
    scope: Option<&TokenScope>,
) -> Result<Departure> {
    let mut out = Departure {
        agent: agent.address.clone(),
        reason: reason.into(),
        ..Default::default()
    };
    let mut stmt = tx.prepare(
        "SELECT token_id FROM tokens WHERE agent_id = ?1 AND revoked_at IS NULL
           AND (?2 OR token_id IN (SELECT token_id FROM remote_agents WHERE agent_id = ?1))",
    )?;
    let tokens = stmt
        .query_map(params![agent.id, all_tokens], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for t in &tokens {
        tx.emit(
            "system",
            "token.revoked",
            json!({ "token_id": t, "reason": reason }),
        )?;
        tx.execute(
            "UPDATE tokens SET revoked_at = ?2 WHERE token_id = ?1",
            params![t, tx.now],
        )?;
    }
    out.tokens_revoked = tokens.len() as u32;
    let holds: Vec<String> = crate::holds::owned_holds_in(tx, &agent.id, scope)?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    crate::holds::release_tx(tx, &agent.address, &holds, reason)?;
    out.holds_released = holds.len() as u32;
    out.leases_closed = crate::resources::session_leases_tx(tx, agent, true, scope)?
        + crate::spend::end_agent_leases_tx(tx, &agent.id, reason, scope)? as u32;
    tx.execute(
        "UPDATE agents SET status = 'offline' WHERE agent_id = ?1",
        [&agent.id],
    )?;
    tx.execute(
        "UPDATE remote_agents SET state = 'gone', left_at = ?2, left_reason = ?3
         WHERE agent_id = ?1 AND state = 'active'",
        params![agent.id, tx.now, reason],
    )?;
    let project_id: Option<String> = tx
        .query_row(
            "SELECT project_id FROM agents WHERE agent_id = ?1",
            [&agent.id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    tx.emit(
        "system",
        if reason == "kicked" {
            "agent.kicked"
        } else {
            "agent.gone"
        },
        json!({ "agent_id": agent.id, "project_id": project_id, "reason": reason }),
    )?;
    // Lanes it belonged to: the invites' lanes and open lanes it leads.
    let mut stmt = tx.prepare(
        "SELECT l.lane_id, l.project_id, l.name FROM lanes l
         WHERE l.node_id = ?2 AND l.closed_at IS NULL AND (l.lead_id = ?1
           OR l.lane_id IN (SELECT lane_id FROM remote_agents WHERE agent_id = ?1))",
    )?;
    let lanes = stmt
        .query_map(params![agent.id, tx.node_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (lane_id, project_id, name) in lanes {
        if let Some(s) = scope {
            // Lanes outside the caller's projects are not its business.
            if !crate::tokens::scope_allows_project(tx, s, &project_id)? {
                continue;
            }
        }
        tx.emit(
            "system",
            "lane.agent_left",
            json!({ "lane_id": lane_id, "project_id": project_id, "name": name, "agent_id": agent.id, "reason": reason }),
        )?;
        let led: bool = tx.query_row(
            "SELECT lead_id = ?2 FROM lanes WHERE lane_id = ?1",
            params![lane_id, agent.id],
            |r| r.get(0),
        )?;
        if led {
            hand_over_lane_tx(tx, &lane_id, &project_id, &name, agent)?;
        }
        out.lanes.push(name);
    }
    Ok(out)
}

/// The lead of an open lane left: the lane member active longest (remote agents joined to
/// the lane, agents holding files for it) becomes lead, and they and the lane's creator are
/// told. With nobody left, the lane stays open with no working lead, flagged
/// `leaderless_since`; it is never blocked automatically.
fn hand_over_lane_tx(
    tx: &Tx<'_>,
    lane_id: &str,
    project_id: &str,
    name: &str,
    left: &AgentRef,
) -> Result<()> {
    let next: Option<String> = tx
        .query_row(
            "SELECT m.agent_id FROM (
               SELECT r.agent_id, MIN(r.joined_at) AS since FROM remote_agents r
               WHERE r.lane_id = ?1 AND r.state = 'active' GROUP BY r.agent_id
               UNION ALL
               SELECT h.agent_id, MIN(h.created_at) FROM holds h
               WHERE h.lane_id = ?1 AND h.released_at IS NULL GROUP BY h.agent_id
             ) m JOIN agents a ON a.agent_id = m.agent_id
             WHERE m.agent_id != ?2 AND a.node_id = ?3
             GROUP BY m.agent_id ORDER BY MIN(m.since), m.agent_id LIMIT 1",
            params![lane_id, left.id, tx.node_id],
            |r| r.get(0),
        )
        .optional()?;
    let creator: String = tx.query_row(
        "SELECT a.address FROM lanes l JOIN agents a ON a.agent_id = l.created_by
         WHERE l.lane_id = ?1",
        [lane_id],
        |r| r.get(0),
    )?;
    let me = crate::identity::system_agent_tx(tx)?;
    // Into the lane's thread when it has one.
    let thread: Option<String> = tx
        .query_row(
            "SELECT l.thread_id FROM lanes l WHERE l.lane_id = ?1
               AND EXISTS (SELECT 1 FROM messages m WHERE m.thread_id = l.thread_id)",
            [lane_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let mail = |to: Vec<String>, subject: String, body: String| -> Result<()> {
        let to: Vec<String> = to.into_iter().filter(|a| *a != left.address).collect();
        if to.is_empty() {
            return Ok(());
        }
        crate::mail::send_tx(
            tx,
            &crate::mail::SendMessage {
                from: me.address.clone(),
                to,
                subject,
                body,
                importance: crate::mail::Importance::High,
                thread_id: thread.clone(),
                ..Default::default()
            },
        )?;
        Ok(())
    };
    match next {
        Some(new_lead) => {
            let lead = agent_by_id(tx, &new_lead)?;
            tx.execute(
                "UPDATE lanes SET lead_id = ?2, updated_at = ?3, leaderless_at = NULL
                 WHERE lane_id = ?1",
                params![lane_id, lead.id, tx.now],
            )?;
            tx.emit(
                &me.address,
                "lane.updated",
                json!({ "lane_id": lane_id, "project_id": project_id, "name": name, "lead_id": lead.id, "reason": "lead_left" }),
            )?;
            mail(
                vec![lead.address.clone(), creator],
                format!("[lane {name}] {} is now the lead", lead.address),
                format!(
                    "{} left the lane, so {} (the longest-active member) now leads it. Change it with `lane update --lead` if that's wrong.",
                    left.address, lead.address
                ),
            )
        }
        None => {
            tx.execute(
                "UPDATE lanes SET leaderless_at = ?2, updated_at = ?2 WHERE lane_id = ?1",
                params![lane_id, tx.now],
            )?;
            tx.emit(
                &me.address,
                "lane.leaderless",
                json!({ "lane_id": lane_id, "project_id": project_id, "name": name, "left_id": left.id }),
            )?;
            mail(
                vec![creator],
                format!("[lane {name}] has no lead"),
                format!(
                    "{} left and nobody else is working in the lane. It stays open; pick a new lead with `lane update --lead`.",
                    left.address
                ),
            )
        }
    }
}

/// Whether `name` may be used by an invite without taking over someone else's identity.
/// Free when no agent on this node has the name, when every agent with it came from this
/// invite (later uses of a multi-use invite refresh it), or when every one is a remote agent
/// that has gone and holds no live token.
fn name_free_for_invite(tx: &Tx<'_>, name: &str, invite_id: Option<&str>) -> Result<bool> {
    let mut stmt = tx.prepare("SELECT agent_id FROM agents WHERE node_id = ?1 AND name = ?2")?;
    let ids = stmt
        .query_map(params![tx.node_id, name], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        let (memberships, active, from_this): (i64, i64, i64) = tx.query_row(
            "SELECT COUNT(*), COUNT(*) FILTER (WHERE state = 'active'),
                    COUNT(*) FILTER (WHERE invite_id = ?2)
             FROM remote_agents WHERE agent_id = ?1",
            params![id, invite_id.unwrap_or("")],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if from_this > 0 && from_this == memberships {
            continue;
        }
        let live_tokens: i64 = tx.query_row(
            "SELECT COUNT(*) FROM tokens WHERE agent_id = ?1 AND revoked_at IS NULL
               AND (expires_at IS NULL OR expires_at > ?2)",
            params![id, tx.now],
            |r| r.get(0),
        )?;
        let departed_remote = memberships > 0 && active == 0 && live_tokens == 0;
        if !departed_remote {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The projects an agent reaches through its active invite tokens, as a scope.
fn membership_scope(tx: &Tx<'_>, agent: &AgentRef) -> Result<TokenScope> {
    let mut stmt = tx.prepare(
        "SELECT t.project_globs FROM remote_agents r JOIN tokens t ON t.token_id = r.token_id
         WHERE r.agent_id = ?1 AND r.state = 'active'",
    )?;
    let mut globs: Vec<String> = Vec::new();
    for g in stmt.query_map([&agent.id], |r| r.get::<_, String>(0))? {
        for x in serde_json::from_str::<Vec<String>>(&g?).unwrap_or_default() {
            if !globs.contains(&x) {
                globs.push(x);
            }
        }
    }
    Ok(TokenScope {
        token_id: String::new(),
        agent_id: agent.id.clone(),
        agent: agent.address.clone(),
        project_globs: globs,
    })
}

/// Ephemeral agents that ended their session or went unseen too long leave (sweeper).
pub(crate) fn expire_remote_agents_tx(tx: &Tx<'_>) -> Result<usize> {
    let mut stmt = tx.prepare(&format!(
        "SELECT DISTINCT r.agent_id,
           CASE WHEN a.status = 'offline' THEN 'session_ended' ELSE 'idle' END
         FROM remote_agents r JOIN agents a ON a.agent_id = r.agent_id
         WHERE r.node_id = ?1 AND r.state = 'active' AND r.ephemeral = 1
           AND (a.status = 'offline'
                OR COALESCE(a.last_seen_at, r.joined_at) < ?2 - {IDLE_HOURS_SQL} * 3600000)"
    ))?;
    let due = stmt
        .query_map(params![tx.node_id, tx.now], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (agent_id, reason) in &due {
        let a = agent_by_id(tx, agent_id)?;
        // Only what the agent did through its invites' tokens is cleaned up: holds and
        // leases it has in other projects (locally) are left alone.
        let scope = membership_scope(tx, &a)?;
        depart_tx(tx, &a, reason, false, Some(&scope))?;
    }
    Ok(due.len())
}

/// Keep a kick's reason on the membership row (free text never goes in an event).
fn record_kick_reason(
    tx: &Tx<'_>,
    a: &AgentRef,
    reason: Option<&str>,
    d: &mut Departure,
) -> Result<()> {
    if let Some(r) = reason {
        let r: String = r.trim().chars().take(200).collect();
        tx.execute(
            "UPDATE remote_agents SET left_reason = ?3
             WHERE agent_id = ?1 AND left_at = ?2 AND left_reason = 'kicked'",
            params![a.id, tx.now, format!("kicked: {r}")],
        )?;
        d.reason = format!("kicked: {r}");
    }
    Ok(())
}

impl Store {
    /// Mint a single-use (by default) invite for a remote agent.
    pub fn create_agent_invite(&self, input: NewAgentInvite) -> Result<CreatedInvite> {
        crate::identity::validate_name(&input.agent)?;
        if input.project_globs.is_empty() || input.project_globs.iter().any(|g| g.trim().is_empty())
        {
            return Err(Error::Invalid(
                "an invite needs at least one --project glob".into(),
            ));
        }
        let secs = input.expires_in_seconds.unwrap_or(DEFAULT_INVITE_SECONDS);
        if !(60..=MAX_INVITE_SECONDS).contains(&secs) {
            return Err(Error::Invalid(
                "invite expiry must be between 1 minute and 30 days".into(),
            ));
        }
        let max_uses = input.max_uses.unwrap_or(1);
        if !(1..=100).contains(&max_uses) {
            return Err(Error::Invalid("--uses must be 1..100".into()));
        }
        // Without --idle-hours, the node's `remote_idle_hours` setting applies (read when
        // checked, so changing it affects existing invites and agents).
        let idle = input.idle_hours;
        if idle.is_some_and(|h| !(1..=720).contains(&h)) {
            return Err(Error::Invalid("--idle-hours must be 1..720".into()));
        }
        if input.token_ttl_seconds.is_some_and(|t| t < 60) {
            return Err(Error::Invalid("token ttl must be at least a minute".into()));
        }
        for g in &input.project_globs {
            globset::GlobBuilder::new(g)
                .literal_separator(true)
                .build()
                .map_err(|e| Error::Invalid(format!("invalid project glob {g:?}: {e}")))?;
        }
        let invite_id = new_id();
        let code = format!("{CODE_PREFIX}{invite_id}_{}", random_hex(24)?);
        self.write(|tx| {
            // An invite never takes over an existing agent (its mail, holds and lanes).
            if !name_free_for_invite(tx, input.agent.trim(), None)? {
                return Err(Error::Conflict(format!(
                    "an agent named {} is already registered here; invite a new name",
                    input.agent.trim()
                )));
            }
            let lane_id = match &input.lane {
                Some(l) => {
                    let (lane, project_id) = resolve_lane(tx, tx.node_id, l)?;
                    if lane.status.is_closed() {
                        return Err(Error::Conflict(format!("lane {} is closed", lane.name)));
                    }
                    let (path, slug): (String, String) = tx.query_row(
                        "SELECT abs_path, slug FROM projects WHERE project_id = ?1",
                        [&project_id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )?;
                    if !glob_allows(&input.project_globs, &[path, slug]) {
                        return Err(Error::Invalid(format!(
                            "lane {} is in a project outside the invite's --project globs",
                            lane.name
                        )));
                    }
                    Some(lane.lane_id)
                }
                None => None,
            };
            tx.execute(
                "INSERT INTO agent_invites (invite_id, node_id, code_hash, agent_name, program,
                   project_globs, lane_id, max_uses, ephemeral, idle_hours, token_ttl, created_at,
                   expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    invite_id,
                    tx.node_id,
                    crate::crypto::sha256_hex(code.as_bytes()),
                    input.agent.trim(),
                    input.program.as_str(),
                    serde_json::to_string(&input.project_globs)?,
                    lane_id,
                    max_uses,
                    input.ephemeral,
                    idle,
                    input.token_ttl_seconds,
                    tx.now,
                    tx.now + secs * 1000
                ],
            )?;
            // Node-local (`invite.*` is never federated or shown to tokens).
            tx.emit(
                "local",
                "invite.created",
                json!({ "invite_id": invite_id, "agent": input.agent.trim(), "lane_id": lane_id, "max_uses": max_uses, "expires_at": tx.now + secs * 1000 }),
            )?;
            let row = tx.query_row(
                &format!("{INVITE_SELECT} WHERE i.invite_id = ?1"),
                [&invite_id],
                invite_from_row(tx.now),
            )?;
            Ok(CreatedInvite {
                invite: row.invite,
                code: code.clone(),
            })
        })
    }

    pub fn agent_invites(&self) -> Result<Vec<AgentInvite>> {
        let now = self.now();
        self.read(|conn| {
            let mut stmt = conn.prepare(&format!(
                "{INVITE_SELECT} WHERE i.node_id = ?1 ORDER BY i.created_at DESC"
            ))?;
            let rows = stmt
                .query_map([self.node_id()], invite_from_row(now))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows.into_iter().map(|r| r.invite).collect())
        })
    }

    /// Stop an invite from being redeemed (issued tokens are kicked separately).
    pub fn revoke_agent_invite(&self, invite_id: &str) -> Result<AgentInvite> {
        self.write(|tx| {
            let n = tx.execute(
                "UPDATE agent_invites SET revoked_at = COALESCE(revoked_at, ?2) WHERE invite_id = ?1",
                params![invite_id.trim().to_ascii_uppercase(), tx.now],
            )?;
            if n == 0 {
                return Err(Error::NotFound(format!("no invite {invite_id:?}")));
            }
            tx.emit("local", "invite.revoked", json!({ "invite_id": invite_id }))?;
            Ok(tx
                .query_row(
                    &format!("{INVITE_SELECT} WHERE i.invite_id = ?1"),
                    [invite_id.trim().to_ascii_uppercase()],
                    invite_from_row(tx.now),
                )?
                .invite)
        })
    }

    /// Redeem an invite code: register the agent and issue its scoped token. Every failure
    /// is the same `forbidden` "invalid or expired invite".
    pub fn redeem_invite(&self, code: &str, client: JoinClient) -> Result<Joined> {
        let invalid = || Error::Forbidden(INVALID.into());
        let id = invite_id_of(code).ok_or_else(invalid)?;
        self.write(|tx| {
            let row = tx
                .query_row(
                    &format!("{INVITE_SELECT} WHERE i.invite_id = ?1 AND i.node_id = ?2"),
                    params![id, tx.node_id],
                    invite_from_row(tx.now),
                )
                .optional()?
                .ok_or_else(invalid)?;
            let given = crate::crypto::sha256_hex(code.trim().as_bytes());
            if !crate::crypto::ct_eq(&given, &row.code_hash)
                || row.invite.revoked_at.is_some()
                || row.expires <= tx.now
                || row.invite.uses >= row.invite.max_uses
            {
                return Err(invalid());
            }
            let inv = &row.invite;
            // Someone registered the name after the invite was made: no takeover.
            if !name_free_for_invite(tx, &inv.agent, Some(&inv.invite_id))? {
                return Err(invalid());
            }
            // Where the agent lives: the lane's project, else the single matching project.
            let lane_project: Option<(String, String)> = match &row.lane_id {
                Some(lane_id) => {
                    let (lane, project_id) = resolve_lane(tx, tx.node_id, lane_id)?;
                    if lane.status.is_closed() {
                        return Err(invalid());
                    }
                    Some((project_id, lane.project_path))
                }
                None => None,
            };
            let project_path = match lane_project {
                Some((_, path)) => Some(path),
                None => match matching_projects(tx, tx.node_id, &inv.project_globs)?.as_slice() {
                    [(_, path)] => Some(path.clone()),
                    _ => None,
                },
            };
            let program: Program = inv.program.parse().unwrap_or(Program::Other);
            let agent = register_tx(
                tx,
                &RegisterAgent {
                    name: inv.agent.clone(),
                    program,
                    project: project_path,
                    model: None,
                    description: Some(format!("remote agent (invite {})", inv.invite_id)),
                },
            )?;
            let token = create_token_tx(
                tx,
                &agent.address,
                &inv.project_globs,
                Some(format!("invite {}", inv.invite_id)),
                inv.token_ttl_seconds,
                "system (invite redeemed)",
            )?;
            tx.execute(
                "UPDATE agent_invites SET uses = uses + 1 WHERE invite_id = ?1",
                [&inv.invite_id],
            )?;
            let raw_idle: Option<u32> = tx.query_row(
                "SELECT idle_hours FROM agent_invites WHERE invite_id = ?1",
                [&inv.invite_id],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO remote_agents (membership_id, node_id, agent_id, invite_id, token_id,
                   lane_id, ephemeral, idle_hours, client_hostname, client_version, joined_at, state)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'active')",
                params![
                    new_id(),
                    tx.node_id,
                    agent.agent_id,
                    inv.invite_id,
                    token.token.token_id,
                    row.lane_id,
                    inv.ephemeral,
                    raw_idle,
                    client.hostname.as_deref().map(|h| h.chars().take(100).collect::<String>()),
                    client.version.as_deref().map(|v| v.chars().take(40).collect::<String>()),
                    tx.now
                ],
            )?;
            let agent_project: Option<String> = tx
                .query_row("SELECT project_id FROM agents WHERE agent_id = ?1", [&agent.agent_id], |r| r.get(0))?;
            tx.emit(
                &agent.address,
                "agent.joined",
                json!({ "agent_id": agent.agent_id, "project_id": agent_project, "lane_id": row.lane_id, "program": inv.program }),
            )?;
            if let Some(lane_id) = &row.lane_id {
                let (lane, project_id) = resolve_lane(tx, tx.node_id, lane_id)?;
                tx.emit(
                    &agent.address,
                    "lane.agent_joined",
                    json!({ "lane_id": lane_id, "project_id": project_id, "name": lane.name, "agent_id": agent.agent_id }),
                )?;
            }
            Ok(Joined {
                invite_id: inv.invite_id.clone(),
                agent: agent.address.clone(),
                agent_id: agent.agent_id.clone(),
                program: inv.program.clone(),
                token: token.secret,
                token_id: token.token.token_id,
                project_globs: inv.project_globs.clone(),
                project: agent.project.clone(),
                lane: inv.lane.clone(),
                ephemeral: inv.ephemeral,
                idle_hours: inv.idle_hours,
            })
        })
    }

    /// Joined agents with presence, newest first.
    pub fn remote_agents(&self) -> Result<Vec<RemoteAgent>> {
        self.sweep()?;
        let now = self.now();
        self.read(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT a.address, a.program, a.last_seen_at, r.joined_at, r.invite_id, r.token_id,
                   l.name, r.ephemeral, {IDLE_HOURS_SQL}, r.client_hostname, r.left_at, r.left_reason,
                   r.state
                 FROM remote_agents r JOIN agents a ON a.agent_id = r.agent_id
                 LEFT JOIN lanes l ON l.lane_id = r.lane_id
                 WHERE r.node_id = ?1 ORDER BY r.joined_at DESC"
            ))?;
            let rows = stmt
                .query_map([self.node_id()], |r| {
                    let last: Option<i64> = r.get(2)?;
                    let state: String = r.get(12)?;
                    let presence = if state == "gone" {
                        Presence::Gone
                    } else if last.is_some_and(|t| now - t <= ONLINE_WINDOW_MS) {
                        Presence::Online
                    } else {
                        Presence::Idle
                    };
                    Ok(RemoteAgent {
                        agent: r.get(0)?,
                        program: r.get(1)?,
                        presence,
                        last_seen_at: last.map(fmt_ts),
                        joined_at: fmt_ts(r.get(3)?),
                        invite_id: r.get(4)?,
                        token_id: r.get(5)?,
                        lane: r.get(6)?,
                        ephemeral: r.get(7)?,
                        idle_hours: r.get(8)?,
                        client_hostname: r.get(9)?,
                        left_at: r.get::<_, Option<i64>>(10)?.map(fmt_ts),
                        left_reason: r.get(11)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Remove an agent now (local only): revoke all its tokens, release its holds and
    /// leases, and tell its lanes.
    pub fn kick_agent(&self, agent: &str, reason: Option<&str>) -> Result<Departure> {
        self.write(|tx| {
            let a = resolve_agent(tx, tx.node_id, agent)?;
            let mut d = depart_tx(tx, &a, "kicked", true, None)?;
            record_kick_reason(tx, &a, reason, &mut d)?;
            Ok(d)
        })
    }

    /// A manager (program `grok_bot`) kicks an agent over MCP. Remote callers act as their
    /// token's agent and may only kick agents in their projects (others are `not_found`).
    /// People and the system agent can't be kicked this way.
    pub fn kick_agent_as(
        &self,
        caller: &str,
        target: &str,
        reason: Option<&str>,
        scope: Option<&TokenScope>,
    ) -> Result<Departure> {
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, caller)?;
            if scope.is_some_and(|s| s.agent_id != me.id) {
                return Err(Error::Forbidden(
                    "act as this token's own agent".into(),
                ));
            }
            let program: String = tx.query_row(
                "SELECT program FROM agents WHERE agent_id = ?1",
                [&me.id],
                |r| r.get(0),
            )?;
            if program != Program::GrokBot.as_str() {
                return Err(Error::Forbidden(
                    "only a manager agent (program grok_bot) may kick over MCP; locally, use `another agent kick`".into(),
                ));
            }
            let unknown = || Error::NotFound(format!("unknown agent {:?}", target.trim()));
            let a = resolve_agent(tx, tx.node_id, target).map_err(|_| unknown())?;
            if let Some(s) = scope {
                let project: Option<String> = tx.query_row(
                    "SELECT project_id FROM agents WHERE agent_id = ?1",
                    [&a.id],
                    |r| r.get(0),
                )?;
                match project {
                    Some(p) if crate::tokens::scope_allows_project(tx, s, &p)? => {}
                    _ => return Err(unknown()),
                }
            }
            if a.id == me.id {
                return Err(Error::Invalid("to leave, use leave, not kick".into()));
            }
            let (name, target_program): (String, String) = tx.query_row(
                "SELECT name, program FROM agents WHERE agent_id = ?1",
                [&a.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if target_program == Program::Human.as_str() || name == crate::identity::SYSTEM_AGENT {
                return Err(Error::Forbidden(
                    "people and the system agent can't be kicked".into(),
                ));
            }
            // A remote manager's kick reaches only into its own projects: the target's holds
            // and leases elsewhere stay (all its tokens are still revoked).
            let mut d = depart_tx(tx, &a, "kicked", true, scope)?;
            record_kick_reason(tx, &a, reason, &mut d)?;
            Ok(d)
        })
    }

    /// The agent leaves: ephemeral agents go `gone` now (tokens revoked, holds and leases
    /// released); persistent ones just end their session.
    pub fn leave(&self, agent: &str) -> Result<Departure> {
        self.leave_scoped(agent, None)
    }

    /// [`Store::leave`] through the agent's own token: only holds and leases inside the
    /// token's projects are released.
    pub fn leave_scoped(&self, agent: &str, scope: Option<&TokenScope>) -> Result<Departure> {
        self.write(|tx| {
            let a = resolve_agent(tx, tx.node_id, agent)?;
            let ephemeral: bool = tx
                .query_row(
                    "SELECT 1 FROM remote_agents WHERE agent_id = ?1 AND state = 'active' AND ephemeral = 1",
                    [&a.id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if ephemeral {
                return depart_tx(tx, &a, "left", false, scope);
            }
            let holds: Vec<String> = crate::holds::owned_holds_in(tx, &a.id, scope)?
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            crate::holds::release_tx(tx, &a.address, &holds, "session_end")?;
            // Like any session end: resource leases are released and spend leases close
            // (in the token's projects); the persistent agent keeps its token.
            let leases_closed = crate::resources::session_leases_tx(tx, &a, true, scope)?
                + crate::spend::end_agent_leases_tx(tx, &a.id, "session_end", scope)? as u32;
            tx.emit(&a.address, "session.ended", json!({ "agent_id": a.id, "client": "remote" }))?;
            tx.execute("UPDATE agents SET status = 'offline' WHERE agent_id = ?1", [&a.id])?;
            Ok(Departure {
                agent: a.address,
                reason: "session ended".into(),
                holds_released: holds.len() as u32,
                leases_closed,
                ..Default::default()
            })
        })
    }
}
