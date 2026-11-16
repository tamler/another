//! Work board: one read-only answer, per project, to "who holds what, what did each agent
//! touch, and what is waiting on whom".
//!
//! Everything comes from rows and events this node already stores: live holds, the
//! `hold.acquired` / `hold.updated` events the edit hook writes (`auto: true`), the
//! `hold.blocked` events it writes when it stops an edit, unread and unacknowledged
//! deliveries, and requests that are still pending. Reading the board writes nothing: no
//! sweep, no event, no projection update. A lapsed hold that no sweep has expired yet is
//! left out by its expiry time; an overdue request that no sweep has marked stale is
//! flagged `overdue`.

use std::collections::{BTreeMap, HashMap, HashSet};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::Value;

use crate::dashboard::presence;
use crate::error::Result;
use crate::identity::find_project;
use crate::store::Store;
use crate::time::{fmt_opt, fmt_ts};

/// The reason the edit hook writes on the holds it takes. Must match
/// `another::hooks::AUTO_HOLD_REASON`; a test in the `another` crate checks that.
pub const EDIT_HOOK_HOLD_REASON: &str = "editing (taken by the edit hook)";

/// Default window for recent edits and stopped edits.
pub const DEFAULT_BOARD_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;
/// Default and largest number of items in each list.
pub const DEFAULT_BOARD_LIMIT: u32 = 20;
pub const MAX_BOARD_LIMIT: u32 = 200;
/// How many edit-hook and stopped-edit events in view one read will scan, newest first.
/// The cap applies after the window, project and edit-hook filters; reaching it sets
/// [`Board::history_truncated`].
pub const BOARD_EVENT_SCAN_CAP: u32 = 5_000;

/// Input for [`Store::board`].
#[derive(Debug, Clone, Default)]
pub struct BoardQuery {
    /// Project slug or absolute path. Sibling checkouts of the same repository are included
    /// unless `confined` is set.
    pub project: Option<String>,
    /// For a caller allowed to see only `project` (a remote connection): leave out sibling
    /// checkouts, and hide agent names, projects and hold patterns that belong only to
    /// other projects (the holder of a stopped edit; request senders, recipients and owners
    /// registered elsewhere). Requires `project`.
    pub confined: bool,
    /// Edits and stopped edits at or after this time (ms). Default: the last 24 hours.
    pub since_ms: Option<i64>,
    /// Items per list (default 20, at most 200).
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Board {
    /// Display name of this node. Holds and edits are this node's only.
    pub node: String,
    pub now: String,
    /// Start of the window for edits and stopped edits.
    pub since: String,
    /// The project filter, when one was given.
    pub project: Option<String>,
    /// Details from outside `project` are hidden (see [`BoardQuery::confined`]).
    pub confined: bool,
    /// The scan of edit-hook and stopped-edit events stopped at [`BOARD_EVENT_SCAN_CAP`];
    /// older edits inside the window may be missing. Narrow `since` or `project`.
    pub history_truncated: bool,
    /// Projects with live holds, recent edits or stopped edits; most recently active first.
    /// A project named by the filter is listed even when it is quiet.
    pub projects: Vec<BoardProject>,
    /// Agents something is waiting on (unread mail, an acknowledgement, a pending request),
    /// plus any agent holding a path.
    pub agents: Vec<BoardAgent>,
    /// Requests still open, claimed or stale; oldest first.
    pub requests: Vec<BoardRequest>,
    /// More pending requests exist than `limit` shows.
    pub requests_truncated: bool,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct BoardProject {
    pub project: String,
    pub path: String,
    /// Live holds, oldest first.
    pub holds: Vec<BoardHold>,
    /// Paths the edit hook claimed for an agent before an edit, newest first, one row per
    /// agent and path. Edits under a hold that was already in place write no event, so this
    /// is the first edit of each path per hold, not every edit.
    pub edits: Vec<BoardEdit>,
    /// Edits the edit hook stopped because another agent held the path, newest first.
    pub blocked: Vec<BoardBlocked>,
    /// `edits` or `blocked` was cut at `limit`.
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct BoardHold {
    pub hold_id: String,
    pub agent: String,
    pub pattern: String,
    pub exclusive: bool,
    pub reason: Option<String>,
    /// Taken by the edit hook rather than asked for.
    pub edit_hook: bool,
    pub since: String,
    pub renewed_at: String,
    pub expires_at: String,
    pub expires_in_seconds: i64,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct BoardEdit {
    pub at: String,
    pub agent: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct BoardBlocked {
    pub at: String,
    /// The agent that was stopped.
    pub agent: String,
    pub path: String,
    /// Who held it, in which project, under which pattern. `None` on a confined board when
    /// the holding project is outside the view.
    pub holder: Option<String>,
    pub holder_project: Option<String>,
    pub pattern: Option<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct BoardAgent {
    pub address: String,
    pub project: Option<String>,
    /// `live`, `idle` or `offline`, the same rule as the dashboard.
    pub presence: String,
    pub last_seen_at: Option<String>,
    pub unread: u32,
    /// Unread and high or urgent.
    pub urgent_unread: u32,
    /// Messages that asked for an acknowledgement and have none from this agent.
    pub awaiting_ack: u32,
    /// Pending requests this agent is named on or owns.
    pub requests: u32,
    pub holds: u32,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct BoardRequest {
    pub request_id: String,
    pub title: String,
    /// `open`, `claimed` or `stale`.
    pub status: String,
    /// Past its due time but not yet marked stale (nothing has swept since).
    pub overdue: bool,
    /// `None` on a confined board when the sender is registered outside the view.
    pub from: Option<String>,
    /// Recipients in view.
    pub to: Vec<String>,
    /// Recipients hidden because they are registered outside the view (confined boards).
    pub to_hidden: u32,
    pub owner: Option<String>,
    /// The request has an owner who is registered outside the view (confined boards).
    pub owner_hidden: bool,
    pub created_at: String,
    pub updated_at: String,
    pub due_at: Option<String>,
}

struct ProjectRow {
    id: String,
    slug: String,
    path: String,
}

struct AgentRow {
    id: String,
    address: String,
    project_id: Option<String>,
    project: Option<String>,
    status: String,
    last_seen_at: Option<i64>,
}

impl Store {
    /// The work board. Reads only; see the module docs for what each part is built from.
    pub fn board(&self, query: BoardQuery) -> Result<Board> {
        let now = self.now();
        let since = query.since_ms.unwrap_or(now - DEFAULT_BOARD_WINDOW_MS);
        let limit = match query.limit {
            0 => DEFAULT_BOARD_LIMIT,
            n => n.min(MAX_BOARD_LIMIT),
        } as usize;
        let local = self.node_id().to_string();
        if query.confined && query.project.is_none() {
            return Err(crate::error::Error::Invalid(
                "a confined board needs a project".into(),
            ));
        }
        self.read(|conn| {
            let node: String = conn.query_row(
                "SELECT display_name FROM nodes WHERE node_id = ?1",
                [&local],
                |r| r.get(0),
            )?;
            let projects = load_projects(conn, &local)?;
            // The projects in view: all of this node's, or the named one and its siblings.
            let scope: Option<HashSet<String>> = match &query.project {
                None => None,
                Some(key) => Some(project_scope(conn, &local, key, !query.confined)?),
            };
            let in_scope = |id: &str| scope.as_ref().map(|s| s.contains(id)).unwrap_or(true);
            let by_id: HashMap<&str, &ProjectRow> =
                projects.iter().map(|p| (p.id.as_str(), p)).collect();
            // Slugs of the projects in view, for details that name a project by slug.
            let slugs_in_view: HashSet<&str> = projects
                .iter()
                .filter(|p| in_scope(&p.id))
                .map(|p| p.slug.as_str())
                .collect();
            // On a confined board, keep a stopped edit's holder only when the holding
            // project is in view.
            let show_project = |slug: &str| !query.confined || slugs_in_view.contains(slug);

            let mut sections: BTreeMap<String, BoardProject> = BTreeMap::new();
            let mut latest: HashMap<String, i64> = HashMap::new();
            let section = |sections: &mut BTreeMap<String, BoardProject>, p: &ProjectRow| {
                sections
                    .entry(p.id.clone())
                    .or_insert_with(|| BoardProject {
                        project: p.slug.clone(),
                        path: p.path.clone(),
                        holds: Vec::new(),
                        edits: Vec::new(),
                        blocked: Vec::new(),
                        truncated: false,
                    });
            };
            if let Some(scope) = &scope {
                for p in projects.iter().filter(|p| scope.contains(&p.id)) {
                    section(&mut sections, p);
                }
            }

            // Live holds: unreleased and not past expiry. No sweep, so a lapsed hold is
            // filtered here instead of being expired.
            let mut holders: HashMap<String, u32> = HashMap::new();
            {
                let mut stmt = conn.prepare(
                    "SELECT h.hold_id, h.project_id, a.agent_id, a.address, h.pattern,
                            h.exclusive, h.reason, h.created_at, h.renewed_at, h.expires_at
                     FROM holds h JOIN agents a ON a.agent_id = h.agent_id
                     WHERE h.node_id = ?1 AND h.released_at IS NULL AND h.expires_at > ?2
                     ORDER BY h.created_at, h.hold_id",
                )?;
                let rows = stmt.query_map(params![local, now], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, bool>(5)?,
                        r.get::<_, Option<String>>(6)?,
                        r.get::<_, i64>(7)?,
                        r.get::<_, i64>(8)?,
                        r.get::<_, i64>(9)?,
                    ))
                })?;
                for row in rows {
                    let (
                        id,
                        project_id,
                        agent_id,
                        agent,
                        pattern,
                        exclusive,
                        reason,
                        created,
                        renewed,
                        expires,
                    ) = row?;
                    let Some(p) = by_id.get(project_id.as_str()) else {
                        continue;
                    };
                    if !in_scope(&project_id) {
                        continue;
                    }
                    section(&mut sections, p);
                    *holders.entry(agent_id).or_default() += 1;
                    bump(&mut latest, &project_id, renewed);
                    let edit_hook = reason.as_deref() == Some(EDIT_HOOK_HOLD_REASON);
                    if let Some(s) = sections.get_mut(&project_id) {
                        s.holds.push(BoardHold {
                            hold_id: id,
                            agent,
                            pattern,
                            exclusive,
                            reason,
                            edit_hook,
                            since: fmt_ts(created),
                            renewed_at: fmt_ts(renewed),
                            expires_at: fmt_ts(expires),
                            expires_in_seconds: (expires - now) / 1000,
                        });
                    }
                }
            }

            // Edits and stopped edits, newest first, from this node's own events. Only events
            // that can appear on this board count toward the scan cap: edit-hook holds and
            // stopped edits, in the window, in a project in view.
            let mut history_truncated = false;
            {
                let scope_json: Option<String> = scope
                    .as_ref()
                    .map(|s| serde_json::to_string(&s.iter().collect::<Vec<_>>()))
                    .transpose()?;
                let mut stmt = conn.prepare(
                    "SELECT ts, actor, kind, payload FROM events
                     WHERE node_id = ?1 AND ts >= ?2
                       AND (kind = 'hold.blocked'
                            OR (kind IN ('hold.acquired', 'hold.updated')
                                AND json_extract(payload, '$.auto') = 1))
                       AND (?3 IS NULL OR json_extract(payload, '$.project_id')
                                          IN (SELECT value FROM json_each(?3)))
                     ORDER BY seq DESC LIMIT ?4",
                )?;
                let rows = stmt
                    .query_map(
                        params![local, since, scope_json, BOARD_EVENT_SCAN_CAP + 1],
                        |r| {
                            Ok((
                                r.get::<_, i64>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, String>(2)?,
                                r.get::<_, String>(3)?,
                            ))
                        },
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                if rows.len() > BOARD_EVENT_SCAN_CAP as usize {
                    history_truncated = true;
                }
                let mut seen: HashSet<(String, String, String)> = HashSet::new();
                for (ts, actor, kind, payload) in
                    rows.into_iter().take(BOARD_EVENT_SCAN_CAP as usize)
                {
                    let p: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
                    let text = |v: &Value, k: &str| {
                        v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
                    };
                    let project_id = text(&p, "project_id");
                    let Some(project) = by_id.get(project_id.as_str()) else {
                        continue;
                    };
                    if !in_scope(&project_id) {
                        continue;
                    }
                    if kind == "hold.blocked" {
                        section(&mut sections, project);
                        bump(&mut latest, &project_id, ts);
                        let s = sections.get_mut(&project_id).expect("section exists");
                        let conflicts = p
                            .get("conflicts")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        for c in &conflicts {
                            if s.blocked.len() >= limit {
                                s.truncated = true;
                                break;
                            }
                            let holder_project = text(c, "project");
                            let shown = show_project(&holder_project);
                            s.blocked.push(BoardBlocked {
                                at: fmt_ts(ts),
                                agent: actor.clone(),
                                path: text(c, "path"),
                                holder: shown.then(|| text(c, "holder")),
                                pattern: shown.then(|| text(c, "pattern")),
                                holder_project: shown.then_some(holder_project),
                            });
                        }
                        continue;
                    }
                    if p.get("auto").and_then(Value::as_bool) != Some(true) {
                        continue;
                    }
                    let path = text(&p, "pattern");
                    if !seen.insert((project_id.clone(), actor.clone(), path.clone())) {
                        continue;
                    }
                    section(&mut sections, project);
                    bump(&mut latest, &project_id, ts);
                    let s = sections.get_mut(&project_id).expect("section exists");
                    if s.edits.len() >= limit {
                        s.truncated = true;
                        continue;
                    }
                    s.edits.push(BoardEdit {
                        at: fmt_ts(ts),
                        agent: actor.clone(),
                        path,
                    });
                }
            }

            let mut out_projects: Vec<(i64, BoardProject)> = sections
                .into_iter()
                .map(|(id, s)| (latest.get(&id).copied().unwrap_or(i64::MIN), s))
                .collect();
            out_projects.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.project.cmp(&b.1.project)));

            // Handoffs: agents of this node with something waiting, and pending requests.
            let agents = load_agents(conn, &local)?;
            let agent_ids: HashSet<&str> = agents
                .iter()
                .filter(|a| scope.is_none() || a.project_id.as_deref().is_some_and(&in_scope))
                .map(|a| a.id.as_str())
                .collect();
            let only = scope.as_ref().map(|_| &agent_ids);
            let (requests, requests_truncated, named) = load_requests(
                conn,
                &local,
                now,
                only,
                if query.confined { only } else { None },
                limit,
            )?;
            let mail = load_mail_counts(conn)?;
            let mut out_agents = Vec::new();
            for a in &agents {
                if !agent_ids.contains(a.id.as_str()) {
                    continue;
                }
                let (unread, urgent_unread, awaiting_ack) =
                    mail.get(&a.id).copied().unwrap_or((0, 0, 0));
                let pending = named.get(&a.id).copied().unwrap_or(0);
                let holds = holders.get(&a.id).copied().unwrap_or(0);
                if unread + awaiting_ack + pending + holds == 0 {
                    continue;
                }
                let last_seen = fmt_opt(a.last_seen_at);
                let (presence, _) = presence(&a.status, last_seen.as_deref(), now);
                out_agents.push(BoardAgent {
                    address: a.address.clone(),
                    project: a.project.clone(),
                    presence: presence.to_string(),
                    last_seen_at: last_seen,
                    unread,
                    urgent_unread,
                    awaiting_ack,
                    requests: pending,
                    holds,
                });
            }
            out_agents.sort_by(|a, b| {
                (b.urgent_unread, b.awaiting_ack + b.requests, b.unread)
                    .cmp(&(a.urgent_unread, a.awaiting_ack + a.requests, a.unread))
                    .then(a.address.cmp(&b.address))
            });

            Ok(Board {
                node,
                now: fmt_ts(now),
                since: fmt_ts(since),
                project: query.project.clone(),
                confined: query.confined,
                history_truncated,
                projects: out_projects.into_iter().map(|(_, s)| s).collect(),
                agents: out_agents,
                requests,
                requests_truncated,
            })
        })
    }
}

fn bump(latest: &mut HashMap<String, i64>, project_id: &str, ts: i64) {
    let slot = latest.entry(project_id.to_string()).or_insert(ts);
    if ts > *slot {
        *slot = ts;
    }
}

fn load_projects(conn: &Connection, local: &str) -> Result<Vec<ProjectRow>> {
    let mut stmt = conn.prepare(
        "SELECT project_id, slug, abs_path FROM projects WHERE node_id = ?1 ORDER BY slug",
    )?;
    let rows = stmt.query_map([local], |r| {
        Ok(ProjectRow {
            id: r.get(0)?,
            slug: r.get(1)?,
            path: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The named project plus, with `siblings`, checkouts already linked to the same
/// repository. The link cache is read as it is; this read never refreshes it.
fn project_scope(
    conn: &Connection,
    local: &str,
    key: &str,
    siblings: bool,
) -> Result<HashSet<String>> {
    let project = find_project(conn, local, key)?;
    if !siblings {
        return Ok(HashSet::from([project.project_id]));
    }
    let repo_key: Option<String> = conn
        .query_row(
            "SELECT repo_key FROM project_repos WHERE project_id = ?1 AND repo_key != ''",
            [&project.project_id],
            |r| r.get(0),
        )
        .optional()?;
    let mut scope = HashSet::from([project.project_id.clone()]);
    if let Some(repo_key) = repo_key {
        let mut stmt = conn.prepare(
            "SELECT pr.project_id FROM project_repos pr
             JOIN projects p ON p.project_id = pr.project_id
             WHERE pr.repo_key = ?1 AND p.node_id = ?2",
        )?;
        for id in stmt.query_map(params![repo_key, local], |r| r.get::<_, String>(0))? {
            scope.insert(id?);
        }
    }
    Ok(scope)
}

fn load_agents(conn: &Connection, local: &str) -> Result<Vec<AgentRow>> {
    let mut stmt = conn.prepare(
        "SELECT a.agent_id, a.address, a.name, a.project_id, p.slug, a.status, a.last_seen_at
         FROM agents a LEFT JOIN projects p ON p.project_id = a.project_id
         WHERE a.node_id = ?1
         ORDER BY a.name",
    )?;
    let rows = stmt.query_map([local], |r| {
        let name: String = r.get(2)?;
        let slug: Option<String> = r.get(4)?;
        let address: Option<String> = r.get(1)?;
        Ok(AgentRow {
            id: r.get(0)?,
            address: address.unwrap_or_else(|| crate::identity::address(&name, slug.as_deref())),
            project_id: r.get(3)?,
            project: slug,
            status: r.get(5)?,
            last_seen_at: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Per agent: unread, urgent unread, awaiting acknowledgement.
fn load_mail_counts(conn: &Connection) -> Result<HashMap<String, (u32, u32, u32)>> {
    let mut stmt = conn.prepare(
        "SELECT d.agent_id,
                count(*) FILTER (WHERE d.read_at IS NULL),
                count(*) FILTER (WHERE d.read_at IS NULL AND m.importance IN ('high', 'urgent')),
                count(*) FILTER (WHERE m.ack_required = 1 AND d.acked_at IS NULL)
         FROM deliveries d JOIN messages m ON m.message_id = d.message_id
         WHERE d.role != 'from'
         GROUP BY d.agent_id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?, r.get(3)?)))
    })?;
    Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
}

type PendingRequests = (Vec<BoardRequest>, bool, HashMap<String, u32>);

/// Pending requests stored on this node, oldest first, and how many each agent is named on
/// (a recipient) or owns. With `only`, a request is kept when its sender, owner or a
/// recipient is one of those agents. With `visible`, participants outside that set are
/// hidden: the sender becomes `None`, recipients are counted in `to_hidden`, and an owner
/// sets `owner_hidden`.
fn load_requests(
    conn: &Connection,
    local: &str,
    now: i64,
    only: Option<&HashSet<&str>>,
    visible: Option<&HashSet<&str>>,
    limit: usize,
) -> Result<PendingRequests> {
    let seen = |id: &str| visible.map(|v| v.contains(id)).unwrap_or(true);
    let mut stmt = conn.prepare(
        "SELECT r.request_id, r.title, r.status, r.due_at, r.created_at, r.updated_at,
                r.created_by, ca.address, r.owner_id, oa.address, r.message_id
         FROM requests r
         JOIN agents ca ON ca.agent_id = r.created_by
         LEFT JOIN agents oa ON oa.agent_id = r.owner_id
         WHERE r.node_id = ?1 AND r.status IN ('open', 'claimed', 'stale')
         ORDER BY r.created_at, r.request_id",
    )?;
    let rows = stmt
        .query_map([local], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, Option<String>>(9)?,
                r.get::<_, String>(10)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut to_stmt = conn.prepare(
        "SELECT d.agent_id, a.address FROM deliveries d
         JOIN agents a ON a.agent_id = d.agent_id
         WHERE d.message_id = ?1 AND d.role = 'to'
         ORDER BY d.rowid",
    )?;
    let mut out = Vec::new();
    let mut truncated = false;
    let mut named: HashMap<String, u32> = HashMap::new();
    for (id, title, status, due, created, updated, from_id, from, owner_id, owner, message_id) in
        rows
    {
        let to = to_stmt
            .query_map([&message_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if let Some(only) = only {
            let touches = only.contains(from_id.as_str())
                || owner_id.as_deref().is_some_and(|o| only.contains(o))
                || to.iter().any(|(a, _)| only.contains(a.as_str()));
            if !touches {
                continue;
            }
        }
        // Claimed work waits on its owner; open or stale work waits on its recipients.
        let mut waiting: Vec<&str> = match &owner_id {
            Some(o) if status == "claimed" => vec![o.as_str()],
            _ => to.iter().map(|(a, _)| a.as_str()).collect(),
        };
        waiting.sort_unstable();
        waiting.dedup();
        for a in waiting {
            *named.entry(a.to_string()).or_default() += 1;
        }
        if out.len() >= limit {
            truncated = true;
            continue;
        }
        let owner_shown = owner_id.as_deref().is_none_or(&seen);
        let (to_shown, to_hidden): (Vec<_>, Vec<_>) =
            to.into_iter().partition(|(a, _)| seen(a.as_str()));
        out.push(BoardRequest {
            request_id: id,
            title,
            overdue: status == "open" && due.is_some_and(|d| d <= now),
            status,
            from: seen(&from_id).then_some(from),
            to: to_shown.into_iter().map(|(_, a)| a).collect(),
            to_hidden: to_hidden.len() as u32,
            owner: if owner_shown { owner } else { None },
            owner_hidden: !owner_shown,
            created_at: fmt_ts(created),
            updated_at: fmt_ts(updated),
            due_at: fmt_opt(due),
        });
    }
    Ok((out, truncated, named))
}
