//! Lanes: named units of work with a charter, a lead agent, a project, a status, linked
//! threads / requests / holds, and structured status reports (see docs/design/01-lanes.md).

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{Error, Result};
use crate::identity::{find_project, get_agent, resolve_agent, resolve_agent_in, AgentRef};
use crate::mail::{send_tx, touch, Importance, SendMessage};
use crate::store::{new_id, Store, Tx};
use crate::time::{fmt_opt, fmt_ts};
use crate::tokens::{scope_allows_project, TokenScope};

const MAX_CHARTER: usize = 16 * 1024;
const MAX_SUMMARY: usize = 2 * 1024;
const MAX_LIST_ITEMS: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LaneStatus {
    Planned,
    Working,
    Blocked,
    NeedsDecision,
    Done,
    Abandoned,
}

impl LaneStatus {
    pub const ALL: [LaneStatus; 6] = [
        LaneStatus::Planned,
        LaneStatus::Working,
        LaneStatus::Blocked,
        LaneStatus::NeedsDecision,
        LaneStatus::Done,
        LaneStatus::Abandoned,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            LaneStatus::Planned => "planned",
            LaneStatus::Working => "working",
            LaneStatus::Blocked => "blocked",
            LaneStatus::NeedsDecision => "needs_decision",
            LaneStatus::Done => "done",
            LaneStatus::Abandoned => "abandoned",
        }
    }

    pub fn is_closed(self) -> bool {
        matches!(self, LaneStatus::Done | LaneStatus::Abandoned)
    }
}

impl std::str::FromStr for LaneStatus {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        LaneStatus::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "unknown lane status {s:?}; expected planned, working, blocked, needs_decision, done or abandoned"
                ))
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LaneLinkKind {
    Thread,
    Request,
    Hold,
    Message,
}

impl LaneLinkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LaneLinkKind::Thread => "thread",
            LaneLinkKind::Request => "request",
            LaneLinkKind::Hold => "hold",
            LaneLinkKind::Message => "message",
        }
    }
}

impl std::str::FromStr for LaneLinkKind {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "thread" => Ok(LaneLinkKind::Thread),
            "request" => Ok(LaneLinkKind::Request),
            "hold" => Ok(LaneLinkKind::Hold),
            "message" => Ok(LaneLinkKind::Message),
            _ => Err(Error::Invalid(format!(
                "unknown link kind {s:?}; expected thread, request, hold or message"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReportKind {
    /// A progress or status update.
    Status,
    /// Work finished: sets the lane to done with an outcome and optional PR link.
    Done,
    /// Written by the stop hook.
    Auto,
}

impl ReportKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ReportKind::Status => "status",
            ReportKind::Done => "done",
            ReportKind::Auto => "auto",
        }
    }
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Lane {
    pub lane_id: String,
    pub name: String,
    pub project: String,
    pub project_path: String,
    pub charter: String,
    pub lead: String,
    pub created_by: String,
    pub status: LaneStatus,
    pub thread_id: Option<String>,
    pub outcome: Option<String>,
    pub pr_url: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub reports: u32,
    pub last_report: Option<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct LaneLink {
    pub kind: LaneLinkKind,
    pub ref_id: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct LaneReport {
    pub report_id: String,
    pub lane_id: String,
    pub agent: String,
    pub kind: ReportKind,
    pub summary: String,
    pub progress: Option<u8>,
    pub status: Option<LaneStatus>,
    pub links: Vec<String>,
    pub artifacts: Vec<String>,
    pub outcome: Option<String>,
    pub pr_url: Option<String>,
    /// The mail copy of this report, if one was sent.
    pub message_id: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct LaneDetail {
    pub lane: Lane,
    pub links: Vec<LaneLink>,
    /// Most recent first (up to 20).
    pub reports: Vec<LaneReport>,
    /// Live path holds taken for this lane.
    #[serde(default)]
    pub holds: Vec<crate::holds::Hold>,
}

#[derive(Debug, Clone)]
pub struct NewLane {
    pub agent: String,
    pub name: String,
    pub project: String,
    pub lead: String,
    pub charter: String,
    pub status: Option<LaneStatus>,
}

#[derive(Debug, Clone, Default)]
pub struct LaneFilter {
    pub status: Option<LaneStatus>,
    pub project: Option<String>,
    pub lead: Option<String>,
    pub include_closed: bool,
}

#[derive(Debug, Clone, Default)]
pub struct LaneUpdate {
    pub agent: String,
    pub lane: String,
    pub status: Option<LaneStatus>,
    pub charter: Option<String>,
    pub lead: Option<String>,
    pub links: Vec<(LaneLinkKind, String)>,
}

#[derive(Debug, Clone)]
pub struct LaneReportInput {
    pub agent: String,
    pub lane: String,
    pub kind: ReportKind,
    pub summary: String,
    pub progress: Option<u8>,
    pub status: Option<LaneStatus>,
    pub links: Vec<String>,
    pub artifacts: Vec<String>,
    pub outcome: Option<String>,
    pub pr_url: Option<String>,
}

fn normalize_name(raw: &str) -> Result<String> {
    let name = raw.trim().to_ascii_lowercase();
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(name)
    } else {
        Err(Error::Invalid(format!(
            "invalid lane name {raw:?}: use 1-64 letters, digits, '-', '_' or '.'"
        )))
    }
}

fn check_len(what: &str, s: &str, max: usize) -> Result<()> {
    if s.len() > max {
        return Err(Error::Invalid(format!("{what} is longer than {max} bytes")));
    }
    Ok(())
}

fn short_list(what: &str, items: &[String]) -> Result<()> {
    if items.len() > MAX_LIST_ITEMS {
        return Err(Error::Invalid(format!(
            "at most {MAX_LIST_ITEMS} {what} per report"
        )));
    }
    for i in items {
        check_len(what, i, 2048)?;
    }
    Ok(())
}

const LANE_SELECT: &str = "SELECT l.lane_id, l.name, p.slug, p.abs_path, l.charter, la.address,
       ca.address, l.status, l.thread_id, l.outcome, l.pr_url, l.created_at, l.updated_at,
       l.closed_at, l.project_id,
       (SELECT count(*) FROM lane_reports r WHERE r.lane_id = l.lane_id),
       (SELECT max(r.created_at) FROM lane_reports r WHERE r.lane_id = l.lane_id)
     FROM lanes l
     JOIN projects p ON p.project_id = l.project_id
     JOIN agents la ON la.agent_id = l.lead_id
     JOIN agents ca ON ca.agent_id = l.created_by";

fn lane_from_row(r: &Row<'_>) -> rusqlite::Result<(Lane, String)> {
    let status: String = r.get(7)?;
    Ok((
        Lane {
            lane_id: r.get(0)?,
            name: r.get(1)?,
            project: r.get(2)?,
            project_path: r.get(3)?,
            charter: r.get(4)?,
            lead: r.get(5)?,
            created_by: r.get(6)?,
            status: status.parse().unwrap_or(LaneStatus::Planned),
            thread_id: r.get(8)?,
            outcome: r.get(9)?,
            pr_url: r.get(10)?,
            created_at: fmt_ts(r.get(11)?),
            updated_at: fmt_ts(r.get(12)?),
            closed_at: fmt_opt(r.get(13)?),
            reports: r.get(15)?,
            last_report: fmt_opt(r.get(16)?),
        },
        r.get(14)?,
    ))
}

/// Find a lane by id, `name` or `name@project` (project slug). Lane names are per project
/// (maintainer's rule): unique among a project's open lanes, so a bare name can match lanes in
/// several projects. Callers with a project in hand use [`resolve_lane_in_project`].
pub(crate) fn resolve_lane(conn: &Connection, node_id: &str, key: &str) -> Result<(Lane, String)> {
    resolve_lane_in(conn, node_id, key, None)
}

/// [`resolve_lane`] as `scope` sees it: lanes outside the scope's projects do not exist for
/// it (the error is the same `not_found` as for an unknown name), and ambiguity is only
/// reported among lanes it may see.
pub(crate) fn resolve_lane_in(
    conn: &Connection,
    node_id: &str,
    key: &str,
    scope: Option<&TokenScope>,
) -> Result<(Lane, String)> {
    let key = key.trim();
    let not_found = || Error::NotFound(format!("unknown lane {key:?}"));
    let visible = |project_id: &str| -> Result<bool> {
        match scope {
            None => Ok(true),
            Some(s) => scope_allows_project(conn, s, project_id),
        }
    };
    let by_id = conn
        .query_row(
            &format!("{LANE_SELECT} WHERE l.lane_id = ?1"),
            [key],
            lane_from_row,
        )
        .optional()?;
    if let Some(found) = by_id {
        return if visible(&found.1)? {
            Ok(found)
        } else {
            Err(not_found())
        };
    }
    let lowered = key.to_ascii_lowercase();
    let (name, project) = match lowered.split_once('@') {
        Some((n, p)) => (n.to_string(), Some(p.to_string())),
        None => (lowered, None),
    };
    let mut stmt = conn.prepare(&format!(
        "{LANE_SELECT} WHERE l.node_id = ?1 AND l.name = ?2 AND (?3 IS NULL OR p.slug = ?3)
         ORDER BY (l.closed_at IS NULL) DESC, l.created_at DESC"
    ))?;
    let rows = stmt
        .query_map(params![node_id, name, project], lane_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // The best match per project: open first, then newest.
    let mut best: Vec<(Lane, String)> = Vec::new();
    for (lane, project_id) in rows {
        if !visible(&project_id)? || best.iter().any(|(_, p)| *p == project_id) {
            continue;
        }
        best.push((lane, project_id));
    }
    match best.len() {
        0 => Err(not_found()),
        1 => Ok(best.remove(0)),
        _ => {
            let projects: Vec<String> = best.iter().map(|(l, _)| l.project.clone()).collect();
            Err(Error::Ambiguous(format!(
                "lane name {name:?} is used in several projects ({}); use {name}@<project>",
                projects.join(", ")
            )))
        }
    }
}

/// Resolve a lane **within one project** (or, without one, within the scope): an id or a
/// name in that project. Lanes elsewhere, outside the scope, or missing all give the same
/// "unknown lane" error, so a lookup can never reveal another project's lane.
pub(crate) fn resolve_lane_in_project(
    conn: &Connection,
    node_id: &str,
    key: &str,
    project_id: Option<&str>,
    scope: Option<&TokenScope>,
) -> Result<(Lane, String)> {
    let unknown = || Error::NotFound(format!("unknown lane {:?}", key.trim()));
    let Some(project_id) = project_id else {
        return resolve_lane_in(conn, node_id, key, scope).map_err(|e| match e {
            Error::Ambiguous(_) => e,
            _ => unknown(),
        });
    };
    let slug: String = conn
        .query_row(
            "SELECT slug FROM projects WHERE project_id = ?1",
            [project_id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(unknown)?;
    let k = key.trim();
    let by_id = conn
        .query_row("SELECT 1 FROM lanes WHERE lane_id = ?1", [k], |_| Ok(()))
        .optional()?
        .is_some();
    let qualified = if by_id || k.contains('@') {
        k.to_string()
    } else {
        format!("{k}@{slug}")
    };
    match resolve_lane_in(conn, node_id, &qualified, scope) {
        Ok((lane, p)) if p == project_id => Ok((lane, p)),
        _ => Err(unknown()),
    }
}

pub(crate) fn check_scope(
    conn: &Connection,
    scope: Option<&TokenScope>,
    project_id: &str,
) -> Result<()> {
    match scope {
        Some(s) if !scope_allows_project(conn, s, project_id)? => Err(Error::Forbidden(
            "this lane is outside the token's project globs".into(),
        )),
        _ => Ok(()),
    }
}

fn report_from_row(r: &Row<'_>) -> rusqlite::Result<LaneReport> {
    let kind: String = r.get(3)?;
    let status: Option<String> = r.get(6)?;
    let links: String = r.get(7)?;
    let artifacts: String = r.get(8)?;
    Ok(LaneReport {
        report_id: r.get(0)?,
        lane_id: r.get(1)?,
        agent: r.get(2)?,
        kind: match kind.as_str() {
            "done" => ReportKind::Done,
            "auto" => ReportKind::Auto,
            _ => ReportKind::Status,
        },
        summary: r.get(4)?,
        progress: r.get(5)?,
        status: status.and_then(|s| s.parse().ok()),
        links: serde_json::from_str(&links).unwrap_or_default(),
        artifacts: serde_json::from_str(&artifacts).unwrap_or_default(),
        outcome: r.get(9)?,
        pr_url: r.get(10)?,
        message_id: r.get(11)?,
        created_at: fmt_ts(r.get(12)?),
    })
}

const REPORT_SELECT: &str = "SELECT r.report_id, r.lane_id, a.address, r.kind, r.summary,
       r.progress, r.status, r.links, r.artifacts, r.outcome, r.pr_url, r.message_id, r.created_at
     FROM lane_reports r JOIN agents a ON a.agent_id = r.agent_id";

fn detail(conn: &Connection, lane: Lane) -> Result<LaneDetail> {
    let mut stmt = conn.prepare(
        "SELECT kind, ref_id, created_at FROM lane_links WHERE lane_id = ?1 ORDER BY created_at",
    )?;
    let links = stmt
        .query_map([&lane.lane_id], |r| {
            let kind: String = r.get(0)?;
            Ok(LaneLink {
                kind: kind.parse().unwrap_or(LaneLinkKind::Message),
                ref_id: r.get(1)?,
                created_at: fmt_ts(r.get(2)?),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut stmt = conn.prepare(&format!(
        "{REPORT_SELECT} WHERE r.lane_id = ?1 ORDER BY r.created_at DESC, r.rowid DESC LIMIT 20"
    ))?;
    let reports = stmt
        .query_map([&lane.lane_id], report_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut stmt = conn.prepare(&format!(
        "{} WHERE h.lane_id = ?1 AND h.released_at IS NULL ORDER BY h.created_at, h.hold_id",
        crate::holds::HOLD_SELECT
    ))?;
    let holds = stmt
        .query_map([&lane.lane_id], crate::holds::hold_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(LaneDetail {
        holds,
        lane,
        links,
        reports,
    })
}

pub(crate) fn set_status(
    tx: &Tx<'_>,
    actor: &AgentRef,
    lane: &Lane,
    project_id: &str,
    to: LaneStatus,
) -> Result<()> {
    if lane.status == to {
        return Ok(());
    }
    if lane.status.is_closed() {
        return Err(Error::Conflict(format!(
            "lane {} is {}; create a new lane to continue",
            lane.name,
            lane.status.as_str()
        )));
    }
    tx.emit(
        &actor.address,
        "lane.status_changed",
        json!({
            "lane_id": lane.lane_id,
            "project_id": project_id,
            "name": lane.name,
            "from": lane.status.as_str(),
            "to": to.as_str(),
        }),
    )?;
    tx.execute(
        "UPDATE lanes SET status = ?2, updated_at = ?3,
           closed_at = CASE WHEN ?4 THEN ?3 ELSE closed_at END
         WHERE lane_id = ?1",
        params![lane.lane_id, to.as_str(), tx.now, to.is_closed()],
    )?;
    Ok(())
}

/// Whether `id` exists **and** belongs to the lane. Messages, threads and requests belong
/// when their sender or a recipient is an agent of the lane's project, or the lane's lead
/// or creator; holds must be in the lane's project.
fn link_in_project(
    tx: &Tx<'_>,
    kind: LaneLinkKind,
    id: &str,
    project_id: &str,
    lane_id: &str,
) -> Result<bool> {
    const PARTY: &str = "EXISTS (SELECT 1 FROM agents a
          WHERE (a.project_id = ?2
                 OR a.agent_id IN (SELECT lead_id FROM lanes WHERE lane_id = ?3)
                 OR a.agent_id IN (SELECT created_by FROM lanes WHERE lane_id = ?3))
            AND (a.agent_id = m.sender_id
                 OR a.agent_id IN (SELECT d.agent_id FROM deliveries d WHERE d.message_id = m.message_id)))";
    let sql = match kind {
        LaneLinkKind::Thread => {
            format!("SELECT 1 FROM messages m WHERE m.thread_id = ?1 AND {PARTY} LIMIT 1")
        }
        LaneLinkKind::Message => {
            format!("SELECT 1 FROM messages m WHERE m.message_id = ?1 AND {PARTY}")
        }
        LaneLinkKind::Request => format!(
            "SELECT 1 FROM requests r JOIN messages m ON m.message_id = r.message_id
             WHERE r.request_id = ?1 AND {PARTY}"
        ),
        LaneLinkKind::Hold => {
            "SELECT 1 FROM holds WHERE hold_id = ?1 AND project_id = ?2 AND ?3 IS NOT NULL".into()
        }
    };
    Ok(tx
        .query_row(&sql, params![id, project_id, lane_id], |_| Ok(()))
        .optional()?
        .is_some())
}

fn add_link(
    tx: &Tx<'_>,
    actor: &AgentRef,
    lane: &Lane,
    project_id: &str,
    kind: LaneLinkKind,
    id: &str,
) -> Result<()> {
    // One error for "does not exist" and "belongs to another project", so linking can't be
    // used to probe other projects.
    if !link_in_project(tx, kind, id, project_id, &lane.lane_id)? {
        return Err(Error::NotFound(format!(
            "no {} {id:?} in lane {}'s project",
            kind.as_str(),
            lane.name
        )));
    }
    let added = tx.execute(
        "INSERT OR IGNORE INTO lane_links (lane_id, kind, ref_id, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![lane.lane_id, kind.as_str(), id, tx.now],
    )?;
    if added > 0 {
        tx.emit(
            &actor.address,
            "lane.linked",
            json!({ "lane_id": lane.lane_id, "project_id": project_id, "name": lane.name, "kind": kind.as_str(), "ref_id": id }),
        )?;
    }
    Ok(())
}

fn render_report_body(input: &LaneReportInput, report_id: &str) -> String {
    let mut out = input.summary.clone();
    if let Some(p) = input.progress {
        out.push_str(&format!("\n\nProgress: {p}%"));
    }
    if let Some(s) = input.status {
        out.push_str(&format!("\nStatus: {}", s.as_str()));
    }
    if let Some(o) = &input.outcome {
        out.push_str(&format!("\nOutcome: {o}"));
    }
    if let Some(pr) = &input.pr_url {
        out.push_str(&format!("\nPR: {pr}"));
    }
    for l in &input.links {
        out.push_str(&format!("\nLink: {l}"));
    }
    for a in &input.artifacts {
        out.push_str(&format!("\nArtifact: {a}"));
    }
    out.push_str(&format!("\n\n(lane report {report_id})"));
    out
}

fn reload(tx: &Tx<'_>, lane_id: &str) -> Result<Lane> {
    Ok(resolve_lane(tx, tx.node_id, lane_id)?.0)
}

impl Store {
    /// Create a lane. Names are unique among open lanes on this node.
    pub fn create_lane(&self, input: NewLane, scope: Option<&TokenScope>) -> Result<LaneDetail> {
        let name = normalize_name(&input.name)?;
        check_len("charter", &input.charter, MAX_CHARTER)?;
        let status = input.status.unwrap_or(LaneStatus::Planned);
        if status.is_closed() {
            return Err(Error::Invalid(
                "a new lane cannot start done or abandoned".into(),
            ));
        }
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, &input.agent)?;
            // Scoped callers can only name leads they may see (hidden = missing).
            let lead = resolve_agent_in(tx, tx.node_id, &input.lead, scope)?;
            let project = find_project(tx, tx.node_id, &input.project)?;
            check_scope(tx, scope, &project.project_id)?;
            let taken = tx
                .query_row(
                    "SELECT 1 FROM lanes WHERE node_id = ?1 AND project_id = ?2 AND name = ?3
                       AND closed_at IS NULL",
                    params![tx.node_id, project.project_id, name],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if taken {
                return Err(Error::Conflict(format!(
                    "an open lane in {} is already named {name:?}", project.slug
                )));
            }
            let lane_id = new_id();
            let event_id = tx.emit(
                &me.address,
                "lane.created",
                json!({
                    "lane_id": lane_id,
                    "project_id": project.project_id,
                    "name": name,
                    "lead_id": lead.id,
                    "status": status.as_str(),
                }),
            )?;
            tx.execute(
                "INSERT INTO lanes (lane_id, node_id, project_id, name, charter, lead_id, created_by,
                   status, created_at, updated_at, event_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10)",
                params![
                    lane_id,
                    tx.node_id,
                    project.project_id,
                    name,
                    input.charter,
                    lead.id,
                    me.id,
                    status.as_str(),
                    tx.now,
                    event_id
                ],
            )?;
            touch(tx, &me.id)?;
            detail(tx, reload(tx, &lane_id)?)
        })
    }

    /// Lanes matching `filter`, newest activity first. With a scope, only lanes whose project
    /// matches the token's globs are returned.
    pub fn lanes(&self, filter: LaneFilter, scope: Option<&TokenScope>) -> Result<Vec<Lane>> {
        self.read(|conn| {
            // A scoped caller's filters never resolve anything node-wide: they are matched
            // against the lanes it may see, so a hidden project or agent behaves exactly like
            // a missing one (an empty list) and no error can name it.
            let (project_id, lead_id) = match scope {
                Some(_) => (None, None),
                None => (
                    match &filter.project {
                        Some(p) => Some(find_project(conn, self.node_id(), p)?.project_id),
                        None => None,
                    },
                    match &filter.lead {
                        Some(a) => Some(resolve_agent(conn, self.node_id(), a)?.id),
                        None => None,
                    },
                ),
            };
            let mut stmt = conn.prepare(&format!(
                "{LANE_SELECT} WHERE l.node_id = ?1 AND (?2 IS NULL OR l.status = ?2)
                   AND (?3 IS NULL OR l.project_id = ?3) AND (?4 IS NULL OR l.lead_id = ?4)
                   AND (?5 OR l.closed_at IS NULL OR ?2 IS NOT NULL)
                 ORDER BY l.updated_at DESC, l.lane_id DESC"
            ))?;
            let rows = stmt
                .query_map(
                    params![
                        self.node_id(),
                        filter.status.map(|s| s.as_str()),
                        project_id,
                        lead_id,
                        filter.include_closed
                    ],
                    lane_from_row,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut out = Vec::new();
            for (lane, project_id) in rows {
                if let Some(s) = scope {
                    if !scope_allows_project(conn, s, &project_id)? {
                        continue;
                    }
                    if let Some(p) = &filter.project {
                        let p = p.trim().trim_end_matches('/');
                        if lane.project != p && lane.project_path != p {
                            continue;
                        }
                    }
                    if let Some(a) = &filter.lead {
                        let a = a.trim();
                        let bare = lane.lead.split('@').next().unwrap_or_default();
                        if lane.lead != a && bare != a {
                            continue;
                        }
                    }
                }
                out.push(lane);
            }
            Ok(out)
        })
    }

    /// One lane with its links and most recent reports.
    pub fn lane(&self, key: &str, scope: Option<&TokenScope>) -> Result<LaneDetail> {
        // Expire lapsed holds first, so the lane's live holds are current without a daemon.
        self.sweep()?;
        self.read(|conn| {
            let (lane, project_id) = resolve_lane_in(conn, self.node_id(), key, scope)?;
            check_scope(conn, scope, &project_id)?;
            detail(conn, lane)
        })
    }

    /// Change status, charter or lead, and attach links.
    pub fn update_lane(&self, input: LaneUpdate, scope: Option<&TokenScope>) -> Result<LaneDetail> {
        if let Some(c) = &input.charter {
            check_len("charter", c, MAX_CHARTER)?;
        }
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, &input.agent)?;
            let (lane, project_id) = resolve_lane_in(tx, tx.node_id, &input.lane, scope)?;
            check_scope(tx, scope, &project_id)?;
            if lane.status.is_closed()
                && (input.charter.is_some() || input.lead.is_some() || input.status.is_some())
            {
                return Err(Error::Conflict(format!(
                    "lane {} is {}",
                    lane.name,
                    lane.status.as_str()
                )));
            }
            let mut changed = serde_json::Map::new();
            if let Some(c) = &input.charter {
                if *c != lane.charter {
                    // The charter itself stays in the lane row; events carry ids only.
                    changed.insert("charter_changed".into(), json!(true));
                    tx.execute(
                        "UPDATE lanes SET charter = ?2, updated_at = ?3 WHERE lane_id = ?1",
                        params![lane.lane_id, c, tx.now],
                    )?;
                }
            }
            if let Some(l) = &input.lead {
                let lead = resolve_agent_in(tx, tx.node_id, l, scope)?;
                changed.insert("lead_id".into(), json!(lead.id));
                tx.execute(
                    "UPDATE lanes SET lead_id = ?2, updated_at = ?3 WHERE lane_id = ?1",
                    params![lane.lane_id, lead.id, tx.now],
                )?;
            }
            if !changed.is_empty() {
                changed.insert("lane_id".into(), json!(lane.lane_id));
                changed.insert("project_id".into(), json!(project_id));
                changed.insert("name".into(), json!(lane.name));
                tx.emit(
                    &me.address,
                    "lane.updated",
                    serde_json::Value::Object(changed),
                )?;
            }
            for (kind, id) in &input.links {
                add_link(tx, &me, &lane, &project_id, *kind, id)?;
            }
            if let Some(to) = input.status {
                if to.is_closed() {
                    return Err(Error::Invalid(
                        "use `lane close` or a done report to finish a lane".into(),
                    ));
                }
                set_status(tx, &me, &lane, &project_id, to)?;
            }
            touch(tx, &me.id)?;
            detail(tx, reload(tx, &lane.lane_id)?)
        })
    }

    /// Post a structured report. It is stored on the lane and mailed to the lane's
    /// counterpart (the creator when the lead reports, otherwise the lead) in the lane thread.
    pub fn report_lane(
        &self,
        input: LaneReportInput,
        scope: Option<&TokenScope>,
    ) -> Result<LaneReport> {
        if input.summary.trim().is_empty() {
            return Err(Error::Invalid("a report needs a summary".into()));
        }
        check_len("summary", &input.summary, MAX_SUMMARY)?;
        short_list("links", &input.links)?;
        short_list("artifacts", &input.artifacts)?;
        if let Some(p) = input.progress {
            if p > 100 {
                return Err(Error::Invalid("progress is a percentage (0-100)".into()));
            }
        }
        if input.kind != ReportKind::Done && (input.outcome.is_some() || input.pr_url.is_some()) {
            return Err(Error::Invalid(
                "outcome and pr_url belong on a done report".into(),
            ));
        }
        if input.kind == ReportKind::Done
            && input.outcome.as_deref().unwrap_or("").trim().is_empty()
        {
            return Err(Error::Invalid("a done report needs an outcome".into()));
        }
        if input.status.is_some_and(|s| s.is_closed()) && input.kind != ReportKind::Done {
            return Err(Error::Invalid(
                "use a done report (or `lane close`) to finish a lane".into(),
            ));
        }
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, &input.agent)?;
            let (lane, project_id) = resolve_lane_in(tx, tx.node_id, &input.lane, scope)?;
            check_scope(tx, scope, &project_id)?;
            if lane.status.is_closed() {
                return Err(Error::Conflict(format!(
                    "lane {} is already {}",
                    lane.name,
                    lane.status.as_str()
                )));
            }
            let report_id = new_id();
            let new_status = match input.kind {
                ReportKind::Done => Some(LaneStatus::Done),
                _ => input.status,
            };

            // Mail copy to the counterpart, in the lane thread.
            let (lead_id, creator_id): (String, String) = tx.query_row(
                "SELECT lead_id, created_by FROM lanes WHERE lane_id = ?1",
                [&lane.lane_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let counterpart = if me.id == lead_id { creator_id } else { lead_id };
            let message = if counterpart != me.id {
                let to = get_agent(tx, &counterpart)?.address;
                let label = new_status.unwrap_or(lane.status).as_str();
                // Subjects show up in the activity feed (and on federated peers), so they carry
                // the lane, status and author only; the summary is in the body.
                let subject: String = format!(
                    "[lane {}] {label}: {} report by {}",
                    lane.name,
                    input.kind.as_str(),
                    me.address
                )
                    .chars()
                    .take(200)
                    .collect();
                let thread_ok = lane.thread_id.as_ref().is_some_and(|t| {
                    tx.query_row("SELECT 1 FROM messages WHERE thread_id = ?1 LIMIT 1", [t], |_| {
                        Ok(())
                    })
                    .optional()
                    .ok()
                    .flatten()
                    .is_some()
                });
                let importance = match (input.kind, new_status) {
                    (_, Some(LaneStatus::NeedsDecision)) | (_, Some(LaneStatus::Blocked)) => {
                        Importance::High
                    }
                    _ => Importance::Normal,
                };
                Some(send_tx(
                    tx,
                    &SendMessage {
                        from: me.address.clone(),
                        to: vec![to],
                        subject,
                        body: render_report_body(&input, &report_id),
                        importance,
                        thread_id: if thread_ok { lane.thread_id.clone() } else { None },
                        ..Default::default()
                    },
                )?)
            } else {
                None
            };
            if let Some(m) = &message {
                if lane.thread_id.is_none() {
                    tx.execute(
                        "UPDATE lanes SET thread_id = ?2 WHERE lane_id = ?1",
                        params![lane.lane_id, m.thread_id],
                    )?;
                }
            }

            let event_id = tx.emit(
                &me.address,
                "lane.reported",
                json!({
                    "lane_id": lane.lane_id,
                    "project_id": project_id,
                    "name": lane.name,
                    "report_id": report_id,
                    "kind": input.kind.as_str(),
                    "status": new_status.map(|s| s.as_str()),
                }),
            )?;
            tx.execute(
                "INSERT INTO lane_reports (report_id, lane_id, node_id, agent_id, kind, summary,
                   progress, status, links, artifacts, outcome, pr_url, message_id, created_at, event_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    report_id,
                    lane.lane_id,
                    tx.node_id,
                    me.id,
                    input.kind.as_str(),
                    input.summary,
                    input.progress,
                    new_status.map(|s| s.as_str()),
                    serde_json::to_string(&input.links)?,
                    serde_json::to_string(&input.artifacts)?,
                    input.outcome,
                    input.pr_url,
                    message.as_ref().map(|m| m.message_id.clone()),
                    tx.now,
                    event_id
                ],
            )?;
            tx.execute(
                "UPDATE lanes SET updated_at = ?2 WHERE lane_id = ?1",
                params![lane.lane_id, tx.now],
            )?;
            if let Some(to) = new_status {
                set_status(tx, &me, &lane, &project_id, to)?;
            }
            if input.kind == ReportKind::Done {
                tx.execute(
                    "UPDATE lanes SET outcome = ?2, pr_url = ?3 WHERE lane_id = ?1",
                    params![lane.lane_id, input.outcome, input.pr_url],
                )?;
                tx.emit(
                    &me.address,
                    "lane.closed",
                    json!({ "lane_id": lane.lane_id, "project_id": project_id, "name": lane.name, "status": "done" }),
                )?;
            }
            touch(tx, &me.id)?;
            Ok(tx.query_row(
                &format!("{REPORT_SELECT} WHERE r.report_id = ?1"),
                [&report_id],
                report_from_row,
            )?)
        })
    }

    /// Close a lane as done (default) or abandoned, without a report.
    pub fn close_lane(
        &self,
        agent: &str,
        lane: &str,
        abandon: bool,
        outcome: Option<String>,
        scope: Option<&TokenScope>,
    ) -> Result<LaneDetail> {
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let (lane, project_id) = resolve_lane_in(tx, tx.node_id, lane, scope)?;
            check_scope(tx, scope, &project_id)?;
            if lane.status.is_closed() {
                return Err(Error::Conflict(format!(
                    "lane {} is {}; create a new lane to continue",
                    lane.name,
                    lane.status.as_str()
                )));
            }
            let to = if abandon {
                LaneStatus::Abandoned
            } else {
                LaneStatus::Done
            };
            set_status(tx, &me, &lane, &project_id, to)?;
            if outcome.is_some() {
                tx.execute(
                    "UPDATE lanes SET outcome = ?2 WHERE lane_id = ?1",
                    params![lane.lane_id, outcome],
                )?;
            }
            tx.emit(
                &me.address,
                "lane.closed",
                json!({ "lane_id": lane.lane_id, "project_id": project_id, "name": lane.name, "status": to.as_str() }),
            )?;
            touch(tx, &me.id)?;
            detail(tx, reload(tx, &lane.lane_id)?)
        })
    }

    /// The single open lane `agent` leads in `project` (used by the stop hook), if exactly one.
    pub fn lead_lane(&self, agent: &str, project: Option<&str>) -> Result<Option<Lane>> {
        let lanes = self.lanes(
            LaneFilter {
                lead: Some(agent.to_string()),
                project: project.map(str::to_string),
                ..Default::default()
            },
            None,
        )?;
        Ok(match lanes.as_slice() {
            [one] => Some(one.clone()),
            _ => None,
        })
    }
}

/// Render a lane as markdown (the old STATUS/DONE file shape).
pub fn lane_markdown(d: &LaneDetail) -> String {
    let l = &d.lane;
    let mut out = format!(
        "# Lane {}: {}\n\n| | |\n|---|---|\n| Status | **{}** |\n| Project | {} |\n| Lead | {} |\n| Created by | {} |\n| Updated | {} |\n",
        l.name,
        l.status.as_str(),
        l.status.as_str(),
        l.project,
        l.lead,
        l.created_by,
        l.updated_at
    );
    if let Some(pr) = &l.pr_url {
        out.push_str(&format!("| PR | {pr} |\n"));
    }
    if let Some(o) = &l.outcome {
        out.push_str(&format!("| Outcome | {o} |\n"));
    }
    out.push_str(&format!("\n## Charter\n\n{}\n", l.charter.trim()));
    if !d.reports.is_empty() {
        out.push_str("\n## Reports (newest first)\n");
        for r in &d.reports {
            out.push_str(&format!(
                "\n### {} · {} · {}{}\n\n{}\n",
                r.created_at,
                r.agent,
                r.kind.as_str(),
                r.progress.map(|p| format!(" · {p}%")).unwrap_or_default(),
                r.summary.trim()
            ));
            for link in &r.links {
                out.push_str(&format!("- link: {link}\n"));
            }
            for a in &r.artifacts {
                out.push_str(&format!("- artifact: {a}\n"));
            }
            if let Some(pr) = &r.pr_url {
                out.push_str(&format!("- PR: {pr}\n"));
            }
        }
    }
    if !d.links.is_empty() {
        out.push_str("\n## Linked\n\n");
        for k in &d.links {
            out.push_str(&format!("- {} `{}`\n", k.kind.as_str(), k.ref_id));
        }
    }
    if !d.holds.is_empty() {
        out.push_str("\n## Holds\n\n");
        for h in &d.holds {
            out.push_str(&format!(
                "- `{}` by {} ({}, until {})\n",
                h.pattern,
                h.agent,
                if h.exclusive { "exclusive" } else { "shared" },
                h.expires_at
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_statuses() {
        assert_eq!(normalize_name(" API-Refactor ").unwrap(), "api-refactor");
        assert!(normalize_name("has space").is_err());
        assert!(normalize_name("").is_err());
        assert_eq!(
            "needs_decision".parse::<LaneStatus>().unwrap(),
            LaneStatus::NeedsDecision
        );
        assert!("finished".parse::<LaneStatus>().is_err());
        assert!(LaneStatus::Abandoned.is_closed());
        assert!(!LaneStatus::Blocked.is_closed());
    }
}
