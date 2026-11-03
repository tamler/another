//! Path holds: advisory, project-scoped leases on files or globs with a TTL.
//!
//! Holds signal intent; they do not lock files. An exclusive hold conflicts with any
//! overlapping hold of another agent; shared holds only conflict with exclusive ones.
//! Holds lapse when their TTL passes without a heartbeat and are then expired by the sweeper.

use std::path::Path;

use globset::{Glob, GlobBuilder, GlobMatcher};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::Serialize;
use serde_json::json;

use crate::error::{Error, Result};
use crate::identity::{address, find_project, resolve_agent, Project};
use crate::mail::touch;
use crate::requests::stale_overdue_tx;
use crate::store::{new_id, Store, Tx};
use crate::time::fmt_ts;

pub const DEFAULT_HOLD_TTL_SECONDS: i64 = 30 * 60;
pub const MIN_HOLD_TTL_SECONDS: i64 = 60;
pub const MAX_HOLD_TTL_SECONDS: i64 = 4 * 60 * 60;

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Hold {
    pub hold_id: String,
    pub project: String,
    pub agent: String,
    /// Path or glob relative to the project root.
    pub pattern: String,
    pub exclusive: bool,
    pub reason: Option<String>,
    pub ttl_seconds: i64,
    pub created_at: String,
    pub expires_at: String,
    /// The lane the hold was taken for (name), if any.
    pub lane: Option<String>,
    pub lane_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct HoldConflict {
    /// The requested path or glob.
    pub path: String,
    /// The existing hold it collides with.
    pub held: Hold,
    /// The overlapping hold is in the requester's own lane, so it does not block
    /// (reported under warnings).
    #[serde(default)]
    pub same_lane: bool,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct HoldReport {
    /// Holds now owned by the caller (empty if any conflict was found).
    pub granted: Vec<Hold>,
    pub conflicts: Vec<HoldConflict>,
    /// Overlapping holds on other nodes in a project with the same name. Holds are node-local,
    /// so these never block; they flag a likely cross-machine clash.
    pub warnings: Vec<HoldConflict>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SweepReport {
    pub expired_holds: usize,
    pub stale_requests: usize,
    pub expired_leases: usize,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct HeartbeatReport {
    pub agent: String,
    pub status: String,
    pub last_seen_at: String,
    pub renewed_holds: Vec<Hold>,
    pub released_holds: Vec<String>,
    /// Resource leases renewed (or released, at session end).
    pub resource_leases: u32,
    pub unread: u32,
    pub urgent_unread: u32,
    /// Requests addressed to or owned by the agent that still need attention.
    pub open_requests: u32,
}

/// Which step of a session a heartbeat represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionPhase {
    Start,
    #[default]
    Beat,
    End,
}

/// Optional context recorded on `session.started` / `session.ended` events.
#[derive(Debug, Clone, Default)]
pub struct SessionInfo {
    /// Client program, e.g. `claude` or `codex`.
    pub client: Option<String>,
    /// The client's own session identifier, if it has one.
    pub session_id: Option<String>,
    /// At session end, keep resource leases (e.g. a server meant to outlive the session).
    pub keep_resources: bool,
}

/// Input for [`Store::acquire_holds`].
#[derive(Debug, Clone)]
pub struct AcquireHolds {
    pub agent: String,
    /// Absolute project path or project slug.
    pub project: String,
    pub paths: Vec<String>,
    pub ttl_seconds: Option<i64>,
    pub exclusive: bool,
    pub reason: Option<String>,
    /// Lane id or name the holds are for. Default: the single open lane the agent leads in
    /// this project, else none.
    pub lane: Option<String>,
}

/// Live holds grouped by lane (see [`Store::holds_by_lane`]).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct LaneHolds {
    /// Lane name; `None` for holds taken outside any lane.
    pub lane: Option<String>,
    pub lane_id: Option<String>,
    pub project: String,
    pub lead: Option<String>,
    pub status: Option<crate::lanes::LaneStatus>,
    pub holds: Vec<Hold>,
}

#[derive(Debug, Clone, Default)]
pub struct HoldFilter {
    pub project: Option<String>,
    pub agent: Option<String>,
}

pub(crate) const HOLD_SELECT: &str =
    "SELECT h.hold_id, p.slug, a.address, NULL, h.pattern, h.exclusive, h.reason,
       h.ttl_seconds, h.created_at, h.expires_at, l.name, h.lane_id
     FROM holds h
     JOIN projects p ON p.project_id = h.project_id
     JOIN agents a ON a.agent_id = h.agent_id
     LEFT JOIN projects ap ON ap.project_id = a.project_id
     LEFT JOIN lanes l ON l.lane_id = h.lane_id";

pub(crate) fn hold_from_row(r: &Row<'_>) -> rusqlite::Result<Hold> {
    let name: String = r.get(2)?;
    let agent_slug: Option<String> = r.get(3)?;
    Ok(Hold {
        hold_id: r.get(0)?,
        project: r.get(1)?,
        agent: address(&name, agent_slug.as_deref()),
        pattern: r.get(4)?,
        exclusive: r.get(5)?,
        reason: r.get(6)?,
        ttl_seconds: r.get(7)?,
        created_at: fmt_ts(r.get(8)?),
        expires_at: fmt_ts(r.get(9)?),
        lane: r.get(10)?,
        lane_id: r.get(11)?,
    })
}

fn live_holds(
    conn: &Connection,
    where_sql: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<Vec<Hold>> {
    let mut stmt = conn.prepare(&format!(
        "{HOLD_SELECT} WHERE h.released_at IS NULL AND {where_sql} ORDER BY h.created_at, h.hold_id"
    ))?;
    let rows = stmt.query_map(args, hold_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn clamp_ttl(ttl: Option<i64>) -> Result<i64> {
    let ttl = ttl.unwrap_or(DEFAULT_HOLD_TTL_SECONDS);
    if ttl <= 0 {
        return Err(Error::Invalid("ttl_seconds must be positive".into()));
    }
    Ok(ttl.clamp(MIN_HOLD_TTL_SECONDS, MAX_HOLD_TTL_SECONDS))
}

/// Normalize a requested path or glob to a project-relative pattern.
pub(crate) fn normalize_pattern(project: &Project, raw: &str) -> Result<String> {
    let raw = raw.trim();
    let root = project.abs_path.trim_end_matches('/');
    let rel = if Path::new(raw).is_absolute() {
        match raw.strip_prefix(root) {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => rest.trim_start_matches('/'),
            _ => {
                return Err(Error::Invalid(format!(
                    "path {raw:?} is outside project {}",
                    project.abs_path
                )))
            }
        }
    } else {
        raw
    };
    let mut parts = Vec::new();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                return Err(Error::Invalid(format!(
                    "path {raw:?} must not contain '..'"
                )))
            }
            p => parts.push(p),
        }
    }
    // An empty pattern means the whole project.
    let pattern = if parts.is_empty() {
        "**".to_string()
    } else {
        parts.join("/")
    };
    compile(&pattern)?;
    Ok(pattern)
}

fn compile(pattern: &str) -> Result<GlobMatcher> {
    let glob: Glob = GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|e| Error::Invalid(format!("invalid glob {pattern:?}: {e}")))?;
    Ok(glob.compile_matcher())
}

fn is_glob(pattern: &str) -> bool {
    pattern.contains(['*', '?', '[', '{'])
}

/// The part of a pattern before its first glob metacharacter.
fn literal_prefix(pattern: &str) -> &str {
    match pattern.find(['*', '?', '[', '{']) {
        Some(i) => &pattern[..i],
        None => pattern,
    }
}

/// `dir` names `path` itself or a directory containing it.
fn covers(dir: &str, path: &str) -> bool {
    path == dir || path.starts_with(&format!("{dir}/"))
}

/// Conservative overlap test between two project-relative patterns. A plain path also
/// covers everything below it, so holding `src` overlaps `src/main.rs`. False positives
/// are acceptable for advisory holds; false negatives are not.
pub fn patterns_overlap(a: &str, b: &str) -> bool {
    if a == b || a == "**" || b == "**" {
        return true;
    }
    match (is_glob(a), is_glob(b)) {
        (false, false) => covers(a, b) || covers(b, a),
        (true, false) => glob_touches_literal(a, b),
        (false, true) => glob_touches_literal(b, a),
        (true, true) => {
            let (pa, pb) = (literal_prefix(a), literal_prefix(b));
            pa.starts_with(pb) || pb.starts_with(pa)
        }
    }
}

fn matches(glob: &str, path: &str) -> bool {
    // Patterns are validated on the way in; treat an unparsable one as overlapping.
    compile(glob).map(|m| m.is_match(path)).unwrap_or(true)
}

fn glob_touches_literal(glob: &str, literal: &str) -> bool {
    if matches(glob, literal) {
        return true;
    }
    // The literal may be a directory with matches of the glob below it: compare the
    // glob's leading components at the literal's depth.
    let comps: Vec<&str> = glob.split('/').collect();
    let lit: Vec<&str> = literal.split('/').collect();
    if let Some(pos) = comps.iter().position(|c| c.contains("**")) {
        if pos < lit.len() {
            return pos == 0 || matches(&comps[..pos].join("/"), &lit[..pos].join("/"));
        }
    }
    comps.len() > lit.len() && matches(&comps[..lit.len()].join("/"), literal)
}

/// Overlapping holds by other agents that clash with the request: `(conflicts, same_lane)`.
/// Clashes with holds in the requester's own lane do not block; they come back separately.
fn lane_conflicts_for(
    tx: &Tx<'_>,
    project_id: &str,
    agent_id: &str,
    patterns: &[(String, String)],
    exclusive: bool,
    lane_id: Option<&str>,
) -> Result<(Vec<HoldConflict>, Vec<HoldConflict>)> {
    let mut blocking = Vec::new();
    let mut same_lane = Vec::new();
    for mut c in conflicts_for(tx, project_id, agent_id, patterns, exclusive)? {
        if lane_id.is_some() && c.held.lane_id.as_deref() == lane_id {
            c.same_lane = true;
            same_lane.push(c);
        } else {
            blocking.push(c);
        }
    }
    Ok((blocking, same_lane))
}

fn conflicts_for(
    tx: &Tx<'_>,
    project_id: &str,
    agent_id: &str,
    patterns: &[(String, String)],
    exclusive: bool,
) -> Result<Vec<HoldConflict>> {
    let mut stmt = tx.prepare(&format!(
        "{HOLD_SELECT} WHERE h.released_at IS NULL AND h.expires_at > ?3
           AND h.project_id = ?1 AND h.agent_id != ?2
         ORDER BY h.created_at"
    ))?;
    let others = stmt
        .query_map(params![project_id, agent_id, tx.now], hold_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut conflicts = Vec::new();
    for (requested, pattern) in patterns {
        for held in &others {
            if (exclusive || held.exclusive) && patterns_overlap(pattern, &held.pattern) {
                conflicts.push(HoldConflict {
                    path: requested.clone(),
                    held: held.clone(),
                    same_lane: false,
                });
            }
        }
    }
    Ok(conflicts)
}

/// Live holds on other nodes, in a project with the same slug, overlapping `patterns`.
fn remote_clashes(
    tx: &Tx<'_>,
    project: &Project,
    patterns: &[(String, String)],
) -> Result<Vec<HoldConflict>> {
    let mut stmt = tx.prepare(&format!(
        "{HOLD_SELECT} WHERE h.released_at IS NULL AND h.expires_at > ?1 AND h.node_id != ?2
           AND p.slug = ?3 ORDER BY h.created_at"
    ))?;
    let theirs = stmt
        .query_map(params![tx.now, tx.node_id, project.slug], hold_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::new();
    for (requested, pattern) in patterns {
        for held in &theirs {
            if patterns_overlap(pattern, &held.pattern) {
                out.push(HoldConflict {
                    path: requested.clone(),
                    held: held.clone(),
                    same_lane: false,
                });
            }
        }
    }
    Ok(out)
}

/// The lane a hold is for: the named one (open, same project), else the single open lane
/// the agent leads in the project, else none.
fn acquire_lane(
    tx: &Tx<'_>,
    me: &crate::identity::AgentRef,
    project_id: &str,
    lane: Option<&str>,
) -> Result<Option<String>> {
    if let Some(key) = lane.map(str::trim).filter(|l| !l.is_empty()) {
        // Holds always have a project, so a bare lane name is looked up in that project only
        // (never ambiguous across, or revealing, other projects).
        let slug: String = tx.query_row(
            "SELECT slug FROM projects WHERE project_id = ?1",
            [project_id],
            |r| r.get(0),
        )?;
        let by_id = tx
            .query_row("SELECT 1 FROM lanes WHERE lane_id = ?1", [key], |_| Ok(()))
            .optional()?
            .is_some();
        let key = if by_id || key.contains('@') {
            key.to_string()
        } else {
            format!("{key}@{slug}")
        };
        let (l, lane_project) = crate::lanes::resolve_lane(tx, tx.node_id, &key)?;
        if lane_project != project_id {
            return Err(Error::Invalid(format!(
                "lane {} belongs to another project",
                l.name
            )));
        }
        if l.status.is_closed() {
            return Err(Error::Conflict(format!("lane {} is closed", l.name)));
        }
        return Ok(Some(l.lane_id));
    }
    let mut stmt = tx.prepare(
        "SELECT lane_id FROM lanes WHERE node_id = ?1 AND project_id = ?2 AND lead_id = ?3
           AND closed_at IS NULL",
    )?;
    let led = stmt
        .query_map(params![tx.node_id, project_id, me.id], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(match led.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    })
}

pub(crate) fn sweep_tx(tx: &Tx<'_>) -> Result<SweepReport> {
    let mut stmt = tx.prepare(
        "SELECT hold_id, pattern, agent_id FROM holds
         WHERE node_id = ?1 AND released_at IS NULL AND expires_at <= ?2",
    )?;
    let expired = stmt
        .query_map(params![tx.node_id, tx.now], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (hold_id, pattern, agent_id) in &expired {
        tx.emit(
            "system",
            "hold.expired",
            json!({ "hold_id": hold_id, "pattern": pattern, "agent_id": agent_id }),
        )?;
        tx.execute(
            "UPDATE holds SET released_at = expires_at, release_reason = 'expired' WHERE hold_id = ?1",
            [hold_id],
        )?;
    }
    let stale_requests = stale_overdue_tx(tx)?;
    let expired_leases = crate::resources::expire_leases_tx(tx)?;
    crate::resources::warn_expiring_leases_tx(tx)?;
    crate::decisions::expire_decisions_tx(tx)?;
    crate::spend::expire_spend_leases_tx(tx)?;
    crate::remote_agents::expire_remote_agents_tx(tx)?;
    Ok(SweepReport {
        expired_holds: expired.len(),
        stale_requests,
        expired_leases,
    })
}

pub(crate) fn release_tx(
    tx: &Tx<'_>,
    actor: &str,
    hold_ids: &[String],
    reason: &str,
) -> Result<()> {
    if hold_ids.is_empty() {
        return Ok(());
    }
    tx.emit(
        actor,
        "hold.released",
        json!({ "hold_ids": hold_ids, "reason": reason }),
    )?;
    for id in hold_ids {
        tx.execute(
            "UPDATE holds SET released_at = ?2, release_reason = ?3 WHERE hold_id = ?1",
            params![id, tx.now, reason],
        )?;
    }
    Ok(())
}

fn renew_tx(tx: &Tx<'_>, actor: &str, holds: &[(String, i64)]) -> Result<()> {
    if holds.is_empty() {
        return Ok(());
    }
    let ids: Vec<&String> = holds.iter().map(|(id, _)| id).collect();
    tx.emit(actor, "hold.renewed", json!({ "hold_ids": ids }))?;
    for (id, ttl) in holds {
        tx.execute(
            "UPDATE holds SET renewed_at = ?2, expires_at = ?2 + ?3 * 1000 WHERE hold_id = ?1",
            params![id, tx.now, ttl],
        )?;
    }
    Ok(())
}

/// Live holds owned by `agent_id`, optionally limited to `only` ids (error on unknown ids).
pub(crate) fn owned_holds(
    tx: &Tx<'_>,
    agent_id: &str,
    only: &[String],
) -> Result<Vec<(String, i64)>> {
    let mut stmt = tx.prepare(
        "SELECT hold_id, ttl_seconds FROM holds
         WHERE agent_id = ?1 AND released_at IS NULL ORDER BY created_at",
    )?;
    let owned = stmt
        .query_map([agent_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if only.is_empty() {
        return Ok(owned);
    }
    only.iter()
        .map(|id| {
            owned
                .iter()
                .find(|(h, _)| h == id)
                .cloned()
                .ok_or_else(|| Error::NotFound(format!("no live hold {id:?} owned by this agent")))
        })
        .collect()
}

fn holds_by_id(conn: &Connection, ids: &[String]) -> Result<Vec<Hold>> {
    ids.iter()
        .map(|id| {
            Ok(conn.query_row(
                &format!("{HOLD_SELECT} WHERE h.hold_id = ?1"),
                [id],
                hold_from_row,
            )?)
        })
        .collect()
}

impl Store {
    /// Expire lapsed holds and mark overdue requests stale.
    pub fn sweep(&self) -> Result<SweepReport> {
        self.write(sweep_tx)
    }

    /// Acquire holds on paths/globs. All-or-nothing: if any requested path conflicts with
    /// another agent's live hold, nothing is granted and the conflicts are reported.
    pub fn acquire_holds(&self, input: AcquireHolds) -> Result<HoldReport> {
        if input.paths.is_empty() {
            return Err(Error::Invalid("no paths given".into()));
        }
        let ttl = clamp_ttl(input.ttl_seconds)?;
        self.write(|tx| {
            sweep_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, &input.agent)?;
            let project = find_project(tx, tx.node_id, &input.project)?;
            let mut patterns: Vec<(String, String)> = Vec::new();
            for raw in &input.paths {
                let p = normalize_pattern(&project, raw)?;
                if !patterns.iter().any(|(_, q)| *q == p) {
                    patterns.push((raw.clone(), p));
                }
            }
            let lane_id = acquire_lane(tx, &me, &project.project_id, input.lane.as_deref())?;
            let (conflicts, same_lane) = lane_conflicts_for(
                tx,
                &project.project_id,
                &me.id,
                &patterns,
                input.exclusive,
                lane_id.as_deref(),
            )?;
            let mut warnings = remote_clashes(tx, &project, &patterns)?;
            warnings.extend(same_lane);
            if !conflicts.is_empty() {
                let mut hold_ids: Vec<&str> = Vec::new();
                let mut holders: Vec<&str> = Vec::new();
                for c in &conflicts {
                    if !hold_ids.contains(&c.held.hold_id.as_str()) {
                        hold_ids.push(&c.held.hold_id);
                    }
                    if !holders.contains(&c.held.agent.as_str()) {
                        holders.push(&c.held.agent);
                    }
                }
                let paths: Vec<&str> = patterns.iter().map(|(_, p)| p.as_str()).collect();
                let mut held_lanes: Vec<&str> = Vec::new();
                for c in &conflicts {
                    if let Some(l) = c.held.lane_id.as_deref() {
                        if !held_lanes.contains(&l) {
                            held_lanes.push(l);
                        }
                    }
                }
                tx.emit(
                    &me.address,
                    "hold.conflict",
                    json!({
                        "agent_id": me.id,
                        "project_id": project.project_id,
                        "paths": paths,
                        "exclusive": input.exclusive,
                        "hold_ids": hold_ids,
                        "holders": holders,
                        "lane_id": lane_id,
                        "held_lane_ids": held_lanes,
                    }),
                )?;
                if let Some(mine) = &lane_id {
                    for other in &held_lanes {
                        let theirs: Vec<&HoldConflict> = conflicts
                            .iter()
                            .filter(|c| c.held.lane_id.as_deref() == Some(*other))
                            .collect();
                        tx.emit(
                            &me.address,
                            "lane.hold_conflict",
                            json!({
                                "lane_id": mine,
                                "other_lane_id": other,
                                "project_id": project.project_id,
                                "agent_id": me.id,
                                "paths": theirs.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(),
                                "held_patterns": theirs.iter().map(|c| c.held.pattern.as_str()).collect::<Vec<_>>(),
                                "holders": theirs.iter().map(|c| c.held.agent.as_str()).collect::<Vec<_>>(),
                            }),
                        )?;
                    }
                }
                touch(tx, &me.id)?;
                return Ok(HoldReport {
                    granted: Vec::new(),
                    conflicts,
                    warnings,
                });
            }
            let mut granted_ids = Vec::new();
            for (_, pattern) in &patterns {
                // Re-acquiring a pattern you already hold updates and renews that hold.
                let existing: Option<String> = tx
                    .query_row(
                        "SELECT hold_id FROM holds WHERE agent_id = ?1 AND project_id = ?2
                           AND pattern = ?3 AND released_at IS NULL",
                        params![me.id, project.project_id, pattern],
                        |r| r.get(0),
                    )
                    .ok();
                let expires_at = tx.now + ttl * 1000;
                let payload = json!({
                    "agent_id": me.id,
                    "project_id": project.project_id,
                    "pattern": pattern,
                    "exclusive": input.exclusive,
                    "reason": input.reason,
                    "ttl_seconds": ttl,
                    "expires_at": expires_at,
                    "lane_id": lane_id,
                });
                let hold_id = match existing {
                    Some(id) => {
                        let mut payload = payload;
                        payload["hold_id"] = json!(id);
                        tx.emit(&me.address, "hold.updated", payload)?;
                        tx.execute(
                            "UPDATE holds SET exclusive = ?2, reason = ?3, ttl_seconds = ?4,
                               renewed_at = ?5, expires_at = ?6, lane_id = ?7 WHERE hold_id = ?1",
                            params![
                                id,
                                input.exclusive,
                                input.reason,
                                ttl,
                                tx.now,
                                expires_at,
                                lane_id
                            ],
                        )?;
                        id
                    }
                    None => {
                        let id = new_id();
                        let mut payload = payload;
                        payload["hold_id"] = json!(id);
                        tx.emit(&me.address, "hold.acquired", payload)?;
                        tx.execute(
                            "INSERT INTO holds (hold_id, node_id, project_id, agent_id, pattern,
                               exclusive, reason, ttl_seconds, created_at, renewed_at, expires_at,
                               lane_id)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10, ?11)",
                            params![
                                id,
                                tx.node_id,
                                project.project_id,
                                me.id,
                                pattern,
                                input.exclusive,
                                input.reason,
                                ttl,
                                tx.now,
                                expires_at,
                                lane_id
                            ],
                        )?;
                        id
                    }
                };
                granted_ids.push(hold_id);
            }
            touch(tx, &me.id)?;
            Ok(HoldReport {
                granted: holds_by_id(tx, &granted_ids)?,
                conflicts: Vec::new(),
                warnings,
            })
        })
    }

    /// Report which of `paths` would conflict for `agent` (no holds are taken).
    pub fn check_holds(
        &self,
        agent: &str,
        project: &str,
        paths: &[String],
        exclusive: bool,
    ) -> Result<Vec<HoldConflict>> {
        self.write(|tx| {
            sweep_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let project = find_project(tx, tx.node_id, project)?;
            let patterns = paths
                .iter()
                .map(|raw| Ok((raw.clone(), normalize_pattern(&project, raw)?)))
                .collect::<Result<Vec<_>>>()?;
            conflicts_for(tx, &project.project_id, &me.id, &patterns, exclusive)
        })
    }

    /// Extend the agent's live holds (all, or just `hold_ids`) by their TTL from now.
    pub fn renew_holds(&self, agent: &str, hold_ids: &[String]) -> Result<Vec<Hold>> {
        self.write(|tx| {
            sweep_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let owned = owned_holds(tx, &me.id, hold_ids)?;
            renew_tx(tx, &me.address, &owned)?;
            touch(tx, &me.id)?;
            let ids: Vec<String> = owned.into_iter().map(|(id, _)| id).collect();
            holds_by_id(tx, &ids)
        })
    }

    /// Release the agent's live holds (all, or just `hold_ids`). Returns the released ids.
    pub fn release_holds(&self, agent: &str, hold_ids: &[String]) -> Result<Vec<String>> {
        self.write(|tx| {
            sweep_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let ids: Vec<String> = owned_holds(tx, &me.id, hold_ids)?
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            release_tx(tx, &me.address, &ids, "released")?;
            touch(tx, &me.id)?;
            Ok(ids)
        })
    }

    /// Live holds, optionally filtered by project and/or agent.
    pub fn holds(&self, filter: HoldFilter) -> Result<Vec<Hold>> {
        self.sweep()?;
        self.read(|conn| {
            let project_id = match &filter.project {
                Some(p) => Some(find_project(conn, self.node_id(), p)?.project_id),
                None => None,
            };
            let agent_id = match &filter.agent {
                Some(a) => Some(resolve_agent(conn, self.node_id(), a)?.id),
                None => None,
            };
            live_holds(
                conn,
                "h.node_id = ?1 AND (?2 IS NULL OR h.project_id = ?2) AND (?3 IS NULL OR h.agent_id = ?3)",
                &[&self.node_id(), &project_id, &agent_id],
            )
        })
    }

    /// Live holds grouped by lane, lanes first (by name), then holds without a lane per
    /// project. Remote callers only see their token's projects.
    pub fn holds_by_lane(
        &self,
        filter: HoldFilter,
        scope: Option<&crate::tokens::TokenScope>,
    ) -> Result<Vec<LaneHolds>> {
        let holds = self.holds(filter)?;
        self.read(|conn| {
            let mut groups: Vec<LaneHolds> = Vec::new();
            for h in holds {
                let project_id: String = conn.query_row(
                    "SELECT project_id FROM holds WHERE hold_id = ?1",
                    [&h.hold_id],
                    |r| r.get(0),
                )?;
                if let Some(s) = scope {
                    if !crate::tokens::scope_allows_project(conn, s, &project_id)? {
                        continue;
                    }
                }
                if let Some(g) = groups
                    .iter_mut()
                    .find(|g| g.lane_id == h.lane_id && g.project == h.project)
                {
                    g.holds.push(h);
                    continue;
                }
                let (lead, status) = match &h.lane_id {
                    Some(id) => {
                        let (l, _) = crate::lanes::resolve_lane(conn, self.node_id(), id)?;
                        (Some(l.lead), Some(l.status))
                    }
                    None => (None, None),
                };
                groups.push(LaneHolds {
                    lane: h.lane.clone(),
                    lane_id: h.lane_id.clone(),
                    project: h.project.clone(),
                    lead,
                    status,
                    holds: vec![h],
                });
            }
            groups.sort_by(|a, b| {
                (a.lane.is_none(), &a.lane, &a.project).cmp(&(
                    b.lane.is_none(),
                    &b.lane,
                    &b.project,
                ))
            });
            Ok(groups)
        })
    }

    /// Session heartbeat: touch `last_seen_at` and renew the agent's holds. With `end`,
    /// release every hold instead and mark the agent offline.
    pub fn heartbeat(&self, agent: &str, end: bool) -> Result<HeartbeatReport> {
        let phase = if end {
            SessionPhase::End
        } else {
            SessionPhase::Beat
        };
        self.session(agent, phase, SessionInfo::default())
    }

    /// Record a session lifecycle step. `Start` and `End` append `session.started` /
    /// `session.ended` events; a plain `Beat` only updates presence and renews holds.
    pub fn session(
        &self,
        agent: &str,
        phase: SessionPhase,
        info: SessionInfo,
    ) -> Result<HeartbeatReport> {
        self.write(|tx| {
            sweep_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let owned = owned_holds(tx, &me.id, &[])?;
            let session_payload = || {
                json!({
                    "agent_id": me.id,
                    "client": info.client,
                    "session_id": info.session_id,
                })
            };
            let release_leases = phase == SessionPhase::End && !info.keep_resources;
            let resource_leases = crate::resources::session_leases_tx(tx, &me, release_leases)?;
            let (renewed, released, status) = if phase == SessionPhase::End {
                let ids: Vec<String> = owned.into_iter().map(|(id, _)| id).collect();
                release_tx(tx, &me.address, &ids, "session_end")?;
                tx.emit(&me.address, "session.ended", session_payload())?;
                tx.execute(
                    "UPDATE agents SET status = 'offline', last_seen_at = ?2 WHERE agent_id = ?1",
                    params![me.id, tx.now],
                )?;
                // An ephemeral remote agent whose session ended leaves now.
                crate::remote_agents::expire_remote_agents_tx(tx)?;
                (Vec::new(), ids, "offline")
            } else {
                if phase == SessionPhase::Start {
                    tx.emit(&me.address, "session.started", session_payload())?;
                }
                renew_tx(tx, &me.address, &owned)?;
                crate::spend::renew_spend_leases_tx(tx, &me.id)?;
                touch(tx, &me.id)?;
                let ids: Vec<String> = owned.into_iter().map(|(id, _)| id).collect();
                (holds_by_id(tx, &ids)?, Vec::new(), "active")
            };
            let (unread, urgent_unread): (u32, u32) = tx.query_row(
                "SELECT count(*), count(*) FILTER (WHERE m.importance IN ('high', 'urgent'))
                 FROM deliveries d JOIN messages m ON m.message_id = d.message_id
                 WHERE d.agent_id = ?1 AND d.read_at IS NULL",
                [&me.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let open_requests = crate::requests::list_requests_for_inbox(tx, &me.id)?.len() as u32;
            Ok(HeartbeatReport {
                agent: me.address,
                status: status.to_string(),
                last_seen_at: fmt_ts(tx.now),
                renewed_holds: renewed,
                released_holds: released,
                resource_leases,
                unread,
                urgent_unread,
                open_requests,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_rules() {
        assert!(patterns_overlap("src/main.rs", "src/main.rs"));
        assert!(patterns_overlap("src", "src/main.rs"));
        assert!(!patterns_overlap("src/main.rs", "src/lib.rs"));
        assert!(!patterns_overlap("src", "srcx/a.rs"));
        assert!(patterns_overlap("src/*.rs", "src/main.rs"));
        assert!(!patterns_overlap("src/*.rs", "src/a/b.rs"));
        assert!(patterns_overlap("src/**", "src/a/b.rs"));
        assert!(patterns_overlap("src/**/*.rs", "src/a"));
        assert!(patterns_overlap("src/**/*.rs", "src"));
        assert!(patterns_overlap("src/**", "src/*.toml"));
        assert!(!patterns_overlap("docs/**", "src/**"));
        assert!(patterns_overlap("**", "anything"));
    }

    #[test]
    fn normalizes_patterns() {
        let project = Project {
            project_id: "p".into(),
            node_id: "n".into(),
            abs_path: "/work/repo".into(),
            slug: "repo".into(),
            created_at: String::new(),
        };
        assert_eq!(normalize_pattern(&project, "./src/").unwrap(), "src");
        assert_eq!(
            normalize_pattern(&project, "/work/repo/src/a.rs").unwrap(),
            "src/a.rs"
        );
        assert_eq!(normalize_pattern(&project, "/work/repo").unwrap(), "**");
        assert!(normalize_pattern(&project, "/work/repository/a").is_err());
        assert!(normalize_pattern(&project, "../x").is_err());
        assert!(normalize_pattern(&project, "src/[").is_err());
    }
}
