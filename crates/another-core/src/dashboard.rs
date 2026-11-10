//! Local dashboard read model.
//!
//! The human view of one node: who is here, what they hold, and what they have said.
//! Message bodies are included only when the caller says the viewer is local. Events
//! that originated on another node stay redacted even then. The activity feed and the
//! audit log never carry bodies, and this module does not change that.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::Value;

use crate::activity::{self, ActivityKind, ActivitySummary, EventRow, Since};
use crate::error::{Error, Result};
use crate::holds::{self, patterns_overlap};
use crate::identity::{agent_from_row, AGENT_COLUMNS};
use crate::requests::{self, Request, RequestStatus};
use crate::resources::{Resource, ResourceFilter};
use crate::store::Store;
use crate::time::fmt_ts;
use crate::Peer;

/// An agent seen within this window, with status `active`, is live.
const LIVE_MS: i64 = 10 * 60 * 1000;
/// An `active` agent quieter than [`LIVE_MS`] but within this window is idle.
const IDLE_MS: i64 = 6 * 60 * 60 * 1000;
/// Offline and last seen longer ago than this: hidden in the dashboard by default.
const STALE_MS: i64 = 24 * 60 * 60 * 1000;
/// Idle at least this long counts as away, so a human may release their hold.
const AWAY_MS: i64 = 30 * 60 * 1000;
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;
const THREAD_LIMIT: usize = 40;
const MESSAGES_PER_THREAD: usize = 40;
const REQUEST_LIST_LIMIT: u32 = 200;
/// How many events a timeline query will scan before it stops.
const SCAN_CAP: u32 = 2_000;

/// What the dashboard is allowed to reveal. The HTTP layer picks this from the bind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DashboardView {
    /// Include bodies of messages authored on this node.
    pub bodies: bool,
    /// Allow the local confirm actions (release a stale hold, expire a request).
    pub actions: bool,
}

impl DashboardView {
    pub const LOCAL: Self = Self {
        bodies: true,
        actions: true,
    };
    pub const REDACTED: Self = Self {
        bodies: false,
        actions: false,
    };
}

#[derive(Debug, Clone, Serialize)]
pub struct DashboardSnapshot {
    pub node: DashNode,
    pub now: String,
    pub seq: i64,
    pub access: DashAccess,
    pub counts: DashCounts,
    pub hour: ActivitySummary,
    pub day: ActivitySummary,
    pub projects: Vec<DashProject>,
    pub agents: Vec<DashAgent>,
    pub holds: Vec<DashHold>,
    pub requests: Vec<DashRequest>,
    pub threads: Vec<DashThread>,
    pub resources: Vec<Resource>,
    pub peers: Vec<Peer>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DashNode {
    pub node_id: String,
    pub display_name: String,
    pub role: String,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DashAccess {
    /// `local` (bodies and actions) or `redacted` (read-only, no bodies).
    pub mode: String,
    pub bodies: bool,
    pub actions: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DashCounts {
    pub live: u32,
    pub idle: u32,
    pub offline: u32,
    pub stale: u32,
    pub holds: u32,
    pub conflicts: u32,
    pub remote_warnings: u32,
    pub requests_open: u32,
    pub requests_claimed: u32,
    pub requests_stale: u32,
    pub requests_done: u32,
    pub requests_cancelled: u32,
    pub resources_live: u32,
    pub resources_lapsed: u32,
    pub peers: u32,
    pub unread: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct DashProject {
    pub slug: String,
    pub path: String,
    pub node: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DashAgent {
    pub agent_id: String,
    pub address: String,
    pub name: String,
    pub project: Option<String>,
    pub program: String,
    pub model: Option<String>,
    pub description: Option<String>,
    /// Stored status: `active` or `offline`.
    pub status: String,
    /// Derived: `live`, `idle` or `offline`.
    pub presence: String,
    /// Offline and not seen for a day. The UI hides these until asked.
    pub stale: bool,
    pub last_seen_at: Option<String>,
    /// Home node name when the agent is not on this node.
    pub node: Option<String>,
    pub local: bool,
    pub unread: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct DashClash {
    pub hold_id: String,
    pub agent: String,
    pub project: String,
    pub pattern: String,
    pub node: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DashHold {
    #[serde(flatten)]
    pub hold: crate::Hold,
    /// A human on this machine may release it (owner away, or it conflicts).
    pub releasable: bool,
    pub conflicts: Vec<DashClash>,
    pub remote_warnings: Vec<DashClash>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DashRequest {
    #[serde(flatten)]
    pub request: Request,
    /// Recipients of the message that opened the request.
    pub to: Vec<String>,
    pub expirable: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DashMessage {
    pub message_id: String,
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub body: Option<String>,
    /// True when a body exists but this view must not show it.
    pub body_redacted: bool,
    pub importance: String,
    pub created_at: String,
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DashThread {
    pub thread_id: String,
    pub subject: String,
    pub participants: Vec<String>,
    pub last_at: String,
    pub messages: Vec<DashMessage>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Default)]
pub struct DashActivityQuery {
    pub text: Option<String>,
    pub agent: Option<String>,
    pub project: Option<String>,
    pub kinds: Vec<ActivityKind>,
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    /// Return events older than this event id.
    pub before_id: Option<String>,
    pub limit: u32,
    pub include_bodies: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DashActivityItem {
    pub event_id: String,
    pub ts: String,
    pub node_id: String,
    pub actor: String,
    pub category: ActivityKind,
    pub kind: String,
    pub summary: String,
    pub refs: Value,
    pub project: Option<String>,
    pub body: Option<String>,
    pub body_redacted: bool,
    /// True when this event was authored on another node.
    pub remote: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DashActivityPage {
    /// Newest first.
    pub items: Vec<DashActivityItem>,
    /// Pass as `before` to read older items.
    pub next_before: Option<String>,
    /// The scan stopped at the cap; older matches may exist outside this page.
    pub scanned_limit: bool,
}

fn parse_ts(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|t| t.timestamp_millis())
}

fn presence(status: &str, last_seen_at: Option<&str>, now: i64) -> (&'static str, bool) {
    let age = last_seen_at
        .and_then(parse_ts)
        .map(|t| now.saturating_sub(t));
    let presence = match (status, age) {
        ("active", Some(age)) if age <= LIVE_MS => "live",
        ("active", Some(age)) if age <= IDLE_MS => "idle",
        // Registered, not yet seen: treat as present, not as a hold to clear.
        ("active", None) => "live",
        _ => "offline",
    };
    let stale = presence == "offline" && age.map(|a| a > STALE_MS).unwrap_or(true);
    (presence, stale)
}

fn away(status: &str, last_seen_at: Option<&str>, now: i64) -> bool {
    let age = last_seen_at
        .and_then(parse_ts)
        .map(|t| now.saturating_sub(t));
    match presence(status, last_seen_at, now).0 {
        "offline" => true,
        "idle" => age.unwrap_or(i64::MAX) >= AWAY_MS,
        _ => false,
    }
}

fn project_of(actor: &str, summary: &str, slugs: &[String]) -> Option<String> {
    if let Some((_, rest)) = actor.split_once('@') {
        let slug = rest.split('@').next().unwrap_or("");
        if !slug.is_empty() && (slugs.is_empty() || slugs.iter().any(|s| s == slug)) {
            return Some(slug.to_string());
        }
    }
    let mut found: Option<&String> = None;
    for slug in slugs {
        let hit = summary.contains(&format!("@{slug}"))
            || summary.contains(&format!(" holds {slug}:"))
            || summary.contains(&format!(" {slug}:"));
        if hit && found.map(|prev| slug.len() > prev.len()).unwrap_or(true) {
            found = Some(slug);
        }
    }
    found.cloned()
}

fn agent_matches(actor: &str, filter: &str) -> bool {
    let filter = filter.trim();
    if filter.is_empty() {
        return true;
    }
    actor == filter || actor.starts_with(&format!("{filter}@"))
}

struct RawMessage {
    message_id: String,
    node_id: String,
    from: String,
    subject: String,
    body: String,
    importance: String,
    created_at: i64,
    request_id: Option<String>,
}

struct ForeignHold {
    hold_id: String,
    agent: String,
    project: String,
    pattern: String,
    node: String,
}

fn expose_body(
    include_bodies: bool,
    message_node: &str,
    local_node: &str,
    body: &str,
) -> (Option<String>, bool) {
    if message_node != local_node {
        (None, true)
    } else if include_bodies {
        (Some(body.to_string()), false)
    } else {
        (None, true)
    }
}

impl Store {
    /// Highest event sequence, for live refresh. `0` when the log is empty.
    pub fn dashboard_seq(&self) -> Result<i64> {
        self.read(|conn| {
            Ok(conn.query_row("SELECT coalesce(max(seq), 0) FROM events", [], |r| r.get(0))?)
        })
    }

    /// One consistent-enough picture of the node for the local dashboard.
    pub fn dashboard_snapshot(&self, view: DashboardView) -> Result<DashboardSnapshot> {
        let now = self.now();
        self.sweep()?;
        let node = self.node()?;
        let holds = self.holds(crate::HoldFilter::default())?;
        let requests = self.requests(crate::RequestFilter {
            limit: REQUEST_LIST_LIMIT,
            ..Default::default()
        })?;
        let resources = self.find_resources(ResourceFilter::default())?;
        let peers = self.peers()?;
        let hour = self.activity_summary(Some(Since::Time(now - HOUR_MS)))?;
        let day = self.activity_summary(Some(Since::Time(now - DAY_MS)))?;
        let extra = self.read(|conn| load_extra(conn, self.node_id(), now, view.bodies))?;

        let mut agents: Vec<DashAgent> = extra
            .agents
            .into_iter()
            .map(|agent| {
                let (presence, stale) = presence(&agent.status, agent.last_seen_at.as_deref(), now);
                DashAgent {
                    unread: extra.unread.get(&agent.agent_id).copied().unwrap_or(0),
                    local: agent.node.is_none(),
                    presence: presence.to_string(),
                    stale,
                    agent_id: agent.agent_id,
                    address: agent.address,
                    name: agent.name,
                    project: agent.project,
                    program: agent.program,
                    model: agent.model,
                    description: agent.description,
                    status: agent.status,
                    last_seen_at: agent.last_seen_at,
                    node: agent.node,
                }
            })
            .collect();
        agents.sort_by(|a, b| {
            presence_rank(&a.presence)
                .cmp(&presence_rank(&b.presence))
                .then(b.last_seen_at.cmp(&a.last_seen_at))
                .then(a.address.cmp(&b.address))
        });

        let by_address: HashMap<&str, &DashAgent> =
            agents.iter().map(|a| (a.address.as_str(), a)).collect();
        let mut dash_holds: Vec<DashHold> = holds
            .iter()
            .map(|hold| DashHold {
                hold: hold.clone(),
                releasable: false,
                conflicts: Vec::new(),
                remote_warnings: Vec::new(),
            })
            .collect();
        let mut conflict_pairs = 0u32;
        for i in 0..holds.len() {
            for j in (i + 1)..holds.len() {
                let (a, b) = (&holds[i], &holds[j]);
                if a.project != b.project || a.agent == b.agent {
                    continue;
                }
                if !(a.exclusive || b.exclusive) || !patterns_overlap(&a.pattern, &b.pattern) {
                    continue;
                }
                conflict_pairs += 1;
                dash_holds[i].conflicts.push(clash_of(b, None));
                dash_holds[j].conflicts.push(clash_of(a, None));
            }
        }
        let mut warning_pairs = 0u32;
        for (i, hold) in holds.iter().enumerate() {
            for foreign in &extra.foreign_holds {
                if foreign.project != hold.project
                    || !patterns_overlap(&hold.pattern, &foreign.pattern)
                {
                    continue;
                }
                warning_pairs += 1;
                dash_holds[i].remote_warnings.push(DashClash {
                    hold_id: foreign.hold_id.clone(),
                    agent: foreign.agent.clone(),
                    project: foreign.project.clone(),
                    pattern: foreign.pattern.clone(),
                    node: Some(foreign.node.clone()),
                });
            }
            let agent = by_address.get(hold.agent.as_str());
            // A cross-node clash is only a warning. Releasing still requires the
            // local owner to be away or stale, even when the hold also overlaps locally.
            // Unknown owner: do not offer Release. A live owner stays, even in a clash.
            let owner_away =
                agent.is_some_and(|a| a.stale || away(&a.status, a.last_seen_at.as_deref(), now));
            dash_holds[i].releasable = owner_away;
        }

        let mut request_counts = BTreeMap::<String, u32>::new();
        for (status, n) in &extra.request_counts {
            request_counts.insert(status.clone(), *n);
        }
        let dash_requests: Vec<DashRequest> = requests
            .into_iter()
            .map(|request| {
                let expirable =
                    matches!(request.status, RequestStatus::Open | RequestStatus::Claimed);
                let to = extra
                    .request_to
                    .get(&request.request_id)
                    .cloned()
                    .unwrap_or_default();
                DashRequest {
                    request,
                    to,
                    expirable,
                }
            })
            .collect();

        let active_peers: Vec<Peer> = peers
            .into_iter()
            .filter(|p| p.revoked_at.is_none())
            .collect();
        let counts = DashCounts {
            live: agents.iter().filter(|a| a.presence == "live").count() as u32,
            idle: agents.iter().filter(|a| a.presence == "idle").count() as u32,
            offline: agents.iter().filter(|a| a.presence == "offline").count() as u32,
            stale: agents.iter().filter(|a| a.stale).count() as u32,
            holds: dash_holds.len() as u32,
            conflicts: conflict_pairs,
            remote_warnings: warning_pairs,
            requests_open: *request_counts.get("open").unwrap_or(&0),
            requests_claimed: *request_counts.get("claimed").unwrap_or(&0),
            requests_stale: *request_counts.get("stale").unwrap_or(&0),
            requests_done: *request_counts.get("done").unwrap_or(&0),
            requests_cancelled: *request_counts.get("cancelled").unwrap_or(&0),
            resources_live: resources.iter().filter(|r| r.status == "live").count() as u32,
            resources_lapsed: resources.iter().filter(|r| r.status == "lapsed").count() as u32,
            peers: active_peers.len() as u32,
            unread: agents.iter().map(|a| a.unread).sum(),
        };

        Ok(DashboardSnapshot {
            node: DashNode {
                node_id: node.node_id,
                display_name: node.display_name,
                role: node.role,
                fingerprint: node.fingerprint,
            },
            now: fmt_ts(now),
            seq: extra.seq,
            access: DashAccess {
                mode: if view.bodies && view.actions {
                    "local"
                } else {
                    "redacted"
                }
                .to_string(),
                bodies: view.bodies,
                actions: view.actions,
            },
            counts,
            hour,
            day,
            projects: extra.projects,
            agents,
            holds: dash_holds,
            requests: dash_requests,
            threads: extra.threads,
            resources,
            peers: active_peers,
        })
    }

    /// Searchable activity, newest first. Bodies only for messages authored on this node,
    /// and only when `include_bodies` is set.
    pub fn dashboard_activity(&self, query: DashActivityQuery) -> Result<DashActivityPage> {
        let limit = match query.limit {
            0 => 80,
            n => n.min(200),
        };
        self.read(|conn| {
            let slugs = project_slugs(conn)?;
            let local = self.node_id();
            let before_seq: Option<i64> = match &query.before_id {
                Some(id) => Some(
                    conn.query_row("SELECT seq FROM events WHERE event_id = ?1", [id], |r| {
                        r.get(0)
                    })
                    .optional()?
                    .ok_or_else(|| Error::NotFound(format!("unknown event id {id:?}")))?,
                ),
                None => None,
            };
            let mut stmt = conn.prepare(
                "SELECT event_id, node_id, ts, actor, kind, payload FROM events
                 WHERE (?1 IS NULL OR ts >= ?1) AND (?2 IS NULL OR ts <= ?2)
                   AND (?3 IS NULL OR seq < ?3)
                 ORDER BY seq DESC LIMIT ?4",
            )?;
            let rows = stmt
                .query_map(
                    params![query.since_ms, query.until_ms, before_seq, SCAN_CAP],
                    |r| {
                        Ok(EventRow {
                            event_id: r.get(0)?,
                            node_id: r.get(1)?,
                            ts: r.get(2)?,
                            actor: r.get(3)?,
                            kind: r.get(4)?,
                            payload: r.get(5)?,
                        })
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let scanned_limit = rows.len() == SCAN_CAP as usize;
            let needle = query
                .text
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_lowercase());
            let agent_filter = query.agent.as_deref().unwrap_or("").trim();
            let project_filter = query.project.as_deref().unwrap_or("").trim();
            let mut matched = Vec::new();
            for row in rows {
                if !query.kinds.is_empty()
                    && !query.kinds.contains(&ActivityKind::of_event(&row.kind))
                {
                    continue;
                }
                if !agent_matches(&row.actor, agent_filter) {
                    continue;
                }
                let rendered = activity::item(conn, row)?;
                let project = project_of(&rendered.actor, &rendered.summary, &slugs);
                if !project_filter.is_empty() && project.as_deref() != Some(project_filter) {
                    continue;
                }
                let (body, body_redacted) =
                    message_body(conn, local, &rendered.refs, query.include_bodies)?;
                if let Some(q) = &needle {
                    let body_hit = query.include_bodies
                        && body
                            .as_deref()
                            .map(|b| b.to_lowercase().contains(q))
                            .unwrap_or(false);
                    let hit = body_hit
                        || rendered.summary.to_lowercase().contains(q)
                        || rendered.actor.to_lowercase().contains(q)
                        || rendered.kind.to_lowercase().contains(q)
                        || project
                            .as_deref()
                            .map(|p| p.to_lowercase().contains(q))
                            .unwrap_or(false);
                    if !hit {
                        continue;
                    }
                }
                matched.push(DashActivityItem {
                    remote: rendered.node_id != local,
                    event_id: rendered.event_id,
                    ts: rendered.ts,
                    node_id: rendered.node_id,
                    actor: rendered.actor,
                    category: rendered.category,
                    kind: rendered.kind,
                    summary: rendered.summary,
                    refs: rendered.refs,
                    project,
                    body,
                    body_redacted,
                });
                if matched.len() > limit as usize {
                    break;
                }
            }
            let more = matched.len() > limit as usize;
            matched.truncate(limit as usize);
            let next_before = if more || (scanned_limit && matched.len() == limit as usize) {
                matched.last().map(|i| i.event_id.clone())
            } else {
                None
            };
            let scanned_limit = scanned_limit && next_before.is_some();
            Ok(DashActivityPage {
                items: matched,
                next_before,
                scanned_limit,
            })
        })
    }

    /// Release one live hold on this node because its owner is away or stale.
    /// A conflict, including a cross-node clash, is not enough while the owner is here.
    /// The event names the owner and records the reason `dashboard`.
    pub fn release_hold_from_dashboard(&self, hold_id: &str) -> Result<()> {
        self.write(|tx| {
            holds::sweep_tx(tx)?;
            let row = hold_row(tx, hold_id)?;
            if row.node_id != tx.node_id {
                return Err(Error::Forbidden(
                    "holds are released on their owning node".into(),
                ));
            }
            // Stale (offline for more than a day) is included: `away` is true for every
            // offline owner, and for an idle owner quiet for at least 30 minutes.
            if !away(&row.status, row.last_seen_at.as_deref(), tx.now) {
                return Err(Error::Conflict(
                    "this hold is still in use; the dashboard releases a hold only when its \
                     owner is away or stale"
                        .into(),
                ));
            }
            holds::release_tx(tx, &row.address, &[hold_id.to_string()], "dashboard")?;
            Ok(())
        })
    }

    /// Mark an open or claimed request stale. Already-stale is a no-op.
    /// A done or cancelled request is left alone.
    pub fn expire_request_from_dashboard(&self, request_id: &str) -> Result<Request> {
        self.write(|tx| {
            requests::stale_overdue_tx(tx)?;
            let (node_id, status): (String, String) = tx
                .query_row(
                    "SELECT node_id, status FROM requests WHERE request_id = ?1",
                    [request_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?
                .ok_or_else(|| Error::NotFound(format!("unknown request {request_id:?}")))?;
            if node_id != tx.node_id {
                return Err(Error::Forbidden(
                    "requests stay on the node that opened them".into(),
                ));
            }
            match status.as_str() {
                "done" | "cancelled" => Err(Error::Conflict(format!(
                    "request {request_id} is already {status}"
                ))),
                "stale" => requests::get_request(tx, request_id),
                "open" | "claimed" => {
                    tx.emit(
                        "dashboard",
                        "request.stale",
                        serde_json::json!({
                            "request_id": request_id,
                            "from": status,
                            "reason": "dashboard",
                        }),
                    )?;
                    tx.execute(
                        "UPDATE requests SET status = 'stale', updated_at = ?2 WHERE request_id = ?1",
                        params![request_id, tx.now],
                    )?;
                    requests::get_request(tx, request_id)
                }
                other => Err(Error::Invalid(format!("unknown request status {other}"))),
            }
        })
    }
}

fn presence_rank(presence: &str) -> u8 {
    match presence {
        "live" => 0,
        "idle" => 1,
        _ => 2,
    }
}

fn clash_of(hold: &crate::Hold, node: Option<String>) -> DashClash {
    DashClash {
        hold_id: hold.hold_id.clone(),
        agent: hold.agent.clone(),
        project: hold.project.clone(),
        pattern: hold.pattern.clone(),
        node,
    }
}

struct Extra {
    agents: Vec<crate::Agent>,
    unread: HashMap<String, u32>,
    projects: Vec<DashProject>,
    threads: Vec<DashThread>,
    foreign_holds: Vec<ForeignHold>,
    request_to: HashMap<String, Vec<String>>,
    request_counts: Vec<(String, u32)>,
    seq: i64,
}

fn load_extra(conn: &Connection, local: &str, now: i64, include_bodies: bool) -> Result<Extra> {
    let agents = {
        let mut stmt = conn.prepare(&format!(
            "SELECT {AGENT_COLUMNS} FROM agents a
             LEFT JOIN projects p ON p.project_id = a.project_id
             ORDER BY a.name"
        ))?;
        let rows = stmt.query_map([], agent_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let unread = {
        let mut stmt = conn.prepare(
            "SELECT agent_id, count(*) FROM deliveries WHERE read_at IS NULL GROUP BY agent_id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?)))?;
        rows.collect::<rusqlite::Result<HashMap<_, _>>>()?
    };
    let projects = {
        let mut stmt = conn.prepare(
            "SELECT p.slug, p.abs_path,
                    CASE WHEN n.is_local = 1 THEN NULL ELSE n.display_name END
             FROM projects p JOIN nodes n ON n.node_id = p.node_id
             ORDER BY n.is_local DESC, p.slug",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(DashProject {
                slug: r.get(0)?,
                path: r.get(1)?,
                node: r.get(2)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let foreign_holds = {
        let mut stmt = conn.prepare(
            "SELECT h.hold_id, a.address, p.slug, h.pattern, n.display_name
             FROM holds h
             JOIN projects p ON p.project_id = h.project_id
             JOIN agents a ON a.agent_id = h.agent_id
             JOIN nodes n ON n.node_id = h.node_id
             WHERE h.released_at IS NULL AND h.expires_at > ?1 AND h.node_id != ?2
             ORDER BY h.created_at",
        )?;
        let rows = stmt.query_map(params![now, local], |r| {
            Ok(ForeignHold {
                hold_id: r.get(0)?,
                agent: r.get(1)?,
                project: r.get(2)?,
                pattern: r.get(3)?,
                node: r.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let request_to = {
        let mut stmt = conn.prepare(
            "SELECT r.request_id, a.address FROM requests r
             JOIN deliveries d ON d.message_id = r.message_id AND d.role = 'to'
             JOIN agents a ON a.agent_id = d.agent_id
             WHERE r.node_id = ?1
             ORDER BY d.rowid",
        )?;
        let rows = stmt.query_map([local], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for row in rows {
            let (id, addr) = row?;
            map.entry(id).or_default().push(addr);
        }
        map
    };
    let request_counts = {
        let mut stmt = conn
            .prepare("SELECT status, count(*) FROM requests WHERE node_id = ?1 GROUP BY status")?;
        let rows = stmt.query_map([local], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let seq: i64 = conn.query_row("SELECT coalesce(max(seq), 0) FROM events", [], |r| r.get(0))?;
    let threads = load_threads(conn, local, include_bodies)?;
    Ok(Extra {
        agents,
        unread,
        projects,
        threads,
        foreign_holds,
        request_to,
        request_counts,
        seq,
    })
}

fn load_threads(conn: &Connection, local: &str, include_bodies: bool) -> Result<Vec<DashThread>> {
    let mut stmt = conn.prepare(
        "SELECT m.thread_id, max(m.created_at),
                (SELECT subject FROM messages f WHERE f.message_id = m.thread_id)
         FROM messages m
         GROUP BY m.thread_id
         ORDER BY max(m.created_at) DESC
         LIMIT ?1",
    )?;
    let threads = stmt
        .query_map([THREAD_LIMIT as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut msg_stmt = conn.prepare(
        "SELECT m.message_id, m.node_id, a.address, m.subject, m.body,
                m.importance, m.created_at,
                (SELECT request_id FROM requests r WHERE r.message_id = m.message_id)
         FROM messages m
         JOIN agents a ON a.agent_id = m.sender_id
         WHERE m.thread_id = ?1
         ORDER BY m.created_at ASC, m.message_id ASC",
    )?;
    let mut del_stmt = conn.prepare(
        "SELECT d.message_id, d.role, a.address FROM deliveries d
         JOIN messages m ON m.message_id = d.message_id
         JOIN agents a ON a.agent_id = d.agent_id
         WHERE m.thread_id = ?1
         ORDER BY d.rowid",
    )?;

    let mut out = Vec::with_capacity(threads.len());
    for (thread_id, last_at, root_subject) in threads {
        let messages = msg_stmt
            .query_map([&thread_id], |r| {
                Ok(RawMessage {
                    message_id: r.get(0)?,
                    node_id: r.get(1)?,
                    from: r.get(2)?,
                    subject: r.get(3)?,
                    body: r.get(4)?,
                    importance: r.get(5)?,
                    created_at: r.get(6)?,
                    request_id: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut to: HashMap<String, Vec<String>> = HashMap::new();
        let mut cc: HashMap<String, Vec<String>> = HashMap::new();
        let deliveries = del_stmt.query_map([&thread_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in deliveries {
            let (id, role, addr) = row?;
            if role == "cc" {
                cc.entry(id).or_default().push(addr);
            } else {
                to.entry(id).or_default().push(addr);
            }
        }
        let truncated = messages.len() > MESSAGES_PER_THREAD;
        let start = messages.len().saturating_sub(MESSAGES_PER_THREAD);
        let mut participants = BTreeSet::new();
        let mut rendered = Vec::new();
        for msg in messages.into_iter().skip(start) {
            participants.insert(msg.from.clone());
            let tos = to.remove(&msg.message_id).unwrap_or_default();
            let ccs = cc.remove(&msg.message_id).unwrap_or_default();
            participants.extend(tos.iter().cloned());
            participants.extend(ccs.iter().cloned());
            let (body, body_redacted) = expose_body(include_bodies, &msg.node_id, local, &msg.body);
            rendered.push(DashMessage {
                message_id: msg.message_id,
                from: msg.from,
                to: tos,
                cc: ccs,
                subject: msg.subject,
                body,
                body_redacted,
                importance: msg.importance,
                created_at: fmt_ts(msg.created_at),
                request_id: msg.request_id,
            });
        }
        let subject = root_subject
            .filter(|s| !s.is_empty())
            .or_else(|| rendered.first().map(|m| m.subject.clone()))
            .unwrap_or_default();
        out.push(DashThread {
            thread_id,
            subject,
            participants: participants.into_iter().collect(),
            last_at: fmt_ts(last_at),
            messages: rendered,
            truncated,
        });
    }
    Ok(out)
}

fn project_slugs(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT DISTINCT slug FROM projects")?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn message_body(
    conn: &Connection,
    local: &str,
    refs: &Value,
    include_bodies: bool,
) -> Result<(Option<String>, bool)> {
    let Some(id) = refs.get("message_id").and_then(Value::as_str) else {
        return Ok((None, false));
    };
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT node_id, body FROM messages WHERE message_id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(match row {
        Some((node, body)) => {
            let (shown, redacted) = expose_body(include_bodies, &node, local, &body);
            (shown, redacted)
        }
        None => (None, false),
    })
}

struct HoldRow {
    node_id: String,
    address: String,
    status: String,
    last_seen_at: Option<String>,
}

fn hold_row(conn: &Connection, hold_id: &str) -> Result<HoldRow> {
    conn.query_row(
        "SELECT h.node_id, a.address, a.status, a.last_seen_at
         FROM holds h
         JOIN agents a ON a.agent_id = h.agent_id
         WHERE h.hold_id = ?1 AND h.released_at IS NULL",
        [hold_id],
        |r| {
            Ok(HoldRow {
                node_id: r.get(0)?,
                address: r.get(1)?,
                status: r.get(2)?,
                last_seen_at: r.get::<_, Option<i64>>(3)?.map(fmt_ts),
            })
        },
    )
    .optional()?
    .ok_or_else(|| Error::NotFound(format!("no live hold {hold_id}")))
}

#[cfg(test)]
mod overlap_tests {
    use super::*;
    use crate::holds::patterns_overlap;
    use crate::{AcquireHolds, Clock, Program, RegisterAgent, Store};

    #[test]
    fn glob_overlap_matches_the_hold_rules() {
        assert!(patterns_overlap("src/**", "src/main.rs"));
        assert!(!patterns_overlap("src/**", "docs/guide.md"));
    }

    #[test]
    fn a_forced_overlap_shows_up_as_a_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let (clock, _handle) = Clock::manual(T0_TEST);
        let store = Store::open_with_clock(dir.path(), clock).unwrap();
        for (name, program) in [("alice", Program::Claude), ("bob", Program::Codex)] {
            store
                .register_agent(RegisterAgent {
                    name: name.into(),
                    program,
                    project: Some("/work/shop".into()),
                    model: None,
                    description: None,
                })
                .unwrap();
        }
        store
            .acquire_holds(AcquireHolds {
                agent: "alice".into(),
                project: "shop".into(),
                paths: vec!["src/**".into()],
                ttl_seconds: Some(3_600),
                exclusive: true,
                reason: None,
            })
            .unwrap();
        let granted = store
            .acquire_holds(AcquireHolds {
                agent: "bob".into(),
                project: "shop".into(),
                paths: vec!["docs/guide.md".into()],
                ttl_seconds: Some(3_600),
                exclusive: true,
                reason: None,
            })
            .unwrap();
        assert_eq!(granted.conflicts.len(), 0);
        let path = store.db_path().unwrap().to_path_buf();
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            let n = conn
                .execute(
                    "UPDATE holds SET pattern = 'src/main.rs' WHERE pattern = 'docs/guide.md'",
                    [],
                )
                .unwrap();
            assert_eq!(n, 1);
        }
        let snap = store.dashboard_snapshot(DashboardView::LOCAL).unwrap();
        assert_eq!(snap.counts.conflicts, 1);
        assert!(snap.holds.iter().all(|h| h.conflicts.len() == 1));
        assert!(
            snap.holds.iter().all(|h| !h.releasable),
            "a live owner is not released just because the hold overlaps"
        );
        let err = store
            .release_hold_from_dashboard(&snap.holds[0].hold.hold_id)
            .unwrap_err();
        assert_eq!(err.code(), "conflict");
    }

    #[test]
    fn active_with_no_last_seen_is_live_and_keeps_the_hold() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store
            .register_agent(RegisterAgent {
                name: "alice".into(),
                program: Program::Claude,
                project: Some("/work/shop".into()),
                model: None,
                description: None,
            })
            .unwrap();
        store
            .acquire_holds(AcquireHolds {
                agent: "alice".into(),
                project: "shop".into(),
                paths: vec!["src/main.rs".into()],
                ttl_seconds: Some(3_600),
                exclusive: true,
                reason: None,
            })
            .unwrap();
        let path = store.db_path().unwrap().to_path_buf();
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute(
                "UPDATE agents SET last_seen_at = NULL, status = 'active' WHERE name = 'alice'",
                [],
            )
            .unwrap();
        }
        let snap = store.dashboard_snapshot(DashboardView::LOCAL).unwrap();
        let alice = snap.agents.iter().find(|a| a.name == "alice").unwrap();
        assert_eq!(alice.presence, "live");
        assert!(!alice.stale);
        assert!(snap.holds.iter().all(|h| !h.releasable));
    }

    const T0_TEST: i64 = 1_800_000_000_000;
}
