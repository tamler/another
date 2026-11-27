//! The work queue of one node: items that are proposed, admitted, claimed and finished.
//!
//! A queue item is not a request. Anyone proposes; only the owner, or a delegate holding a
//! key, admits; `next` is the only way into `claimed` and hands one ready item to one agent
//! inside one write transaction. Each transition and note is an event and, in the same
//! transaction, a row in `queue_history`.
//!
//! The authority checks stop an agent acting by habit or accident. They are not a security
//! boundary: any local process can write the database file (docs/QUEUE.md).

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::error::{Error, Result};
use crate::holds::SessionInfo;
use crate::identity::{resolve_agent, resolve_any, AgentRef};
use crate::mail::{is_unsafe_text_char, touch, SendMessage};
use crate::store::{new_id, Store, Tx};
use crate::time::fmt_ts;

/// The limit of claimed, unfinished items for an agent the owner has set no limit for.
pub const DEFAULT_QUEUE_LIMIT: i64 = 1;
/// The highest limit the owner can set. 0 means no seat.
pub const MAX_QUEUE_LIMIT: i64 = 20;
/// Historical operator repair grace only. Participating claims use lease_policy.
pub const DEFAULT_QUEUE_GRACE_SECONDS: i64 = 86_400;
/// Historical operator repair bound; never applied to a projected actor claim.
pub const MIN_QUEUE_GRACE_SECONDS: i64 = 2 * 3600;
pub const MAX_QUEUE_GRACE_SECONDS: i64 = 30 * 86_400;

/// A claim, or a reserved agent, is quiet after this long with no sign of life.
const QUIET_MS: i64 = 3_600_000;
/// How long the agent that gave an item up is not offered it again.
const SKIP_MS: i64 = 3_600_000;
const FAILED_NOTICE_WINDOW_MS: i64 = 24 * 3_600_000;
const SELF_ADMITTED_RECENT_MS: i64 = 7 * 24 * 3_600_000;

const MAX_TITLE_CHARS: usize = 200;
const MAX_SCOPE_CHARS: usize = 32;
const MAX_BODY_BYTES: usize = 10_000;
const MAX_DONE_CONDITION_CHARS: usize = 1000;
const MAX_NOTE_CHARS: usize = 2000;
const MAX_REASON_CHARS: usize = 500;
const MAX_SUMMARY_CHARS: usize = 500;
const MAX_POINTER_CHARS: usize = 500;

/// The clients whose hooks are relied on to send the same session id at session start, at
/// each prompt, after each tool call and at session end. Only their sessions get a
/// `queue_sessions` row, so only their claims are ever bound to a session; a claim made
/// under any other client is unbound and returns only by the grace period or an admitter.
/// Filled from the builder check recorded in docs/HOOKS.md.
pub const QUEUE_SESSION_CLIENTS: [&str; 1] = ["claude"];
/// A session row not seen for this long is deleted by the queue sweep.
const SESSION_ROW_KEPT_MS: i64 = 30 * 86_400_000;
/// The longest session id the queue stores. Client ids are far shorter.
const MAX_SESSION_ID_BYTES: usize = 256;

const AUTOMATIC: &str = "(Automatic notice from Another.)";
const FAST_LANE_IS_THE_OWNERS: &str =
    "security and data-loss items are decided by the owner personally";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QueueState {
    Proposed,
    Ready,
    Claimed,
    Done,
}

impl QueueState {
    pub fn as_str(self) -> &'static str {
        match self {
            QueueState::Proposed => "proposed",
            QueueState::Ready => "ready",
            QueueState::Claimed => "claimed",
            QueueState::Done => "done",
        }
    }
}

impl std::str::FromStr for QueueState {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "proposed" => Ok(QueueState::Proposed),
            "ready" => Ok(QueueState::Ready),
            "claimed" => Ok(QueueState::Claimed),
            "done" => Ok(QueueState::Done),
            _ => Err(Error::Invalid(format!(
                "unknown queue state {s:?}; expected proposed, ready, claimed or done"
            ))),
        }
    }
}

/// What an item is. `Security` and `DataLoss` are the fast lane: only the owner decides them.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum QueueKind {
    #[default]
    Work,
    Security,
    DataLoss,
}

impl QueueKind {
    pub fn as_str(self) -> &'static str {
        match self {
            QueueKind::Work => "work",
            QueueKind::Security => "security",
            QueueKind::DataLoss => "data_loss",
        }
    }

    /// The word the command line uses for this kind.
    pub fn flag(self) -> &'static str {
        match self {
            QueueKind::DataLoss => "data-loss",
            other => other.as_str(),
        }
    }

    pub fn is_fast_lane(self) -> bool {
        self != QueueKind::Work
    }
}

impl std::str::FromStr for QueueKind {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "work" => Ok(QueueKind::Work),
            "security" => Ok(QueueKind::Security),
            "data_loss" | "data-loss" => Ok(QueueKind::DataLoss),
            _ => Err(Error::Invalid(format!(
                "unknown queue kind {s:?}; expected work, security or data-loss"
            ))),
        }
    }
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum QueuePriority {
    High,
    #[default]
    Normal,
}

impl QueuePriority {
    pub fn as_str(self) -> &'static str {
        match self {
            QueuePriority::High => "high",
            QueuePriority::Normal => "normal",
        }
    }

    fn rank(self) -> i64 {
        match self {
            QueuePriority::High => 1,
            QueuePriority::Normal => 2,
        }
    }

    fn of_rank(rank: i64) -> QueuePriority {
        if rank == 1 {
            QueuePriority::High
        } else {
            QueuePriority::Normal
        }
    }
}

impl std::str::FromStr for QueuePriority {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "high" => Ok(QueuePriority::High),
            "normal" => Ok(QueuePriority::Normal),
            _ => Err(Error::Invalid(format!(
                "unknown priority {s:?}; expected high or normal"
            ))),
        }
    }
}

/// What a result points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PointerKind {
    Pr,
    Commit,
    File,
}

impl PointerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PointerKind::Pr => "pr",
            PointerKind::Commit => "commit",
            PointerKind::File => "file",
        }
    }
}

/// Who is acting. The command line builds `Owner` only after the typed yes at a terminal;
/// no MCP tool builds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueCaller {
    /// The owner, in person.
    Owner,
    /// A local agent. `admit_key` is the delegate key it presented, if any.
    Agent {
        agent: String,
        admit_key: Option<String>,
    },
}

impl QueueCaller {
    /// An agent with no delegate key.
    pub fn agent(name: &str) -> QueueCaller {
        QueueCaller::Agent {
            agent: name.to_string(),
            admit_key: None,
        }
    }
}

/// Input for [`Store::queue_propose`].
#[derive(Debug, Clone, Default)]
pub struct ProposeItem {
    pub title: String,
    pub body: String,
    pub scope: Option<String>,
    pub done_when: Option<String>,
    pub kind: QueueKind,
}

/// Input for [`Store::queue_admit`]: what the admitter sets as it admits.
#[derive(Debug, Clone, Default)]
pub struct AdmitItem {
    pub scope: Option<String>,
    pub done_when: Option<String>,
    pub priority: Option<QueuePriority>,
    /// Reserve the item for this agent: a first offer, not a restriction.
    pub for_agent: Option<String>,
}

/// Input for [`Store::queue_edit`].
#[derive(Debug, Clone, Default)]
pub struct EditItem {
    pub scope: Option<String>,
    pub done_when: Option<String>,
    pub priority: Option<QueuePriority>,
    pub for_agent: Option<String>,
    /// Clear the reservation.
    pub for_anyone: bool,
    pub kind: Option<QueueKind>,
}

/// Input for [`Store::queue_release`].
#[derive(Debug, Clone, Default)]
pub struct ReleaseItem {
    /// The claimer's note.
    pub note: Option<String>,
    /// The claimer hands the item to this agent.
    pub to: Option<String>,
    /// An admitter's reason for releasing another agent's claim.
    pub reason: Option<String>,
}

/// Input for [`Store::queue_done`]: a short summary and one pointer.
#[derive(Debug, Clone)]
pub struct DoneItem {
    pub summary: String,
    pub pointer_kind: PointerKind,
    pub pointer: String,
}

/// Which items [`Store::queue_list`] returns. With no state, `mine` or `self_admitted`:
/// the ready and claimed ones.
#[derive(Debug, Clone, Default)]
pub struct QueueFilter {
    pub state: Option<QueueState>,
    pub scope: Option<String>,
    /// Items this agent proposed or holds, in any state unless `state` is given.
    pub mine: Option<String>,
    /// Every self-admitted item, in any state unless `state` is given.
    pub self_admitted: bool,
    /// Maximum items (default 100).
    pub limit: u32,
}

/// The approval record of an admitted item.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct QueueAdmitted {
    /// `owner` or `delegate`.
    pub authority: String,
    /// The delegate's address, or `owner`.
    pub by: String,
    pub at: String,
    /// A delegate admitted its own proposal.
    pub self_admitted: bool,
    /// The last edit of the scope or done condition after admission.
    pub edited_after: Option<QueueEdited>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct QueueEdited {
    pub by: String,
    pub at: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct QueueClaim {
    pub agent: String,
    pub at: String,
    /// The claim is tied to one session, whose end releases it.
    pub session_bound: bool,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct QueueResult {
    /// `completed` or `rejected`.
    pub outcome: String,
    /// The result summary, or the reason for a rejection.
    pub summary: Option<String>,
    /// `pr`, `commit` or `file`.
    pub pointer_kind: Option<String>,
    pub pointer: Option<String>,
    pub by: String,
    pub at: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct QueueHistoryEntry {
    pub at: String,
    /// The event kind, for example `queue.claimed`.
    pub kind: String,
    /// An agent address, `owner` or `system`.
    pub actor: String,
    pub note: Option<String>,
    pub detail: Option<Value>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct QueueItem {
    pub item_id: String,
    pub seq: i64,
    /// The display alias on its home node, for example `Q12`.
    pub label: String,
    pub node_id: String,
    pub state: QueueState,
    pub kind: QueueKind,
    pub title: String,
    pub body: String,
    pub scope: Option<String>,
    pub done_condition: Option<String>,
    pub priority: QueuePriority,
    pub for_agent: Option<String>,
    pub skip_agent: Option<String>,
    /// An agent address, or `owner`.
    pub proposed_by: String,
    pub proposed_at: String,
    pub admitted: Option<QueueAdmitted>,
    pub claim: Option<QueueClaim>,
    pub result: Option<QueueResult>,
    /// Every transition and note, oldest first. Left out of lists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<QueueHistoryEntry>>,
}

impl QueueItem {
    /// Who admitted the item and when, as `queue show` prints it, with a second line when
    /// the scope or done condition was edited afterwards. `None` until it is admitted.
    pub fn approval_line(&self) -> Option<String> {
        let a = self.admitted.as_ref()?;
        let mut line = match (a.authority.as_str(), a.self_admitted) {
            ("delegate", true) => {
                format!("admitted by {} (delegate, self-admitted) at {}", a.by, a.at)
            }
            ("delegate", false) => format!("admitted by {} (delegate) at {}", a.by, a.at),
            _ => format!("admitted by {} at {}", a.by, a.at),
        };
        if let Some(e) = &a.edited_after {
            line.push_str(&format!("\nedited by {} at {} after admission", e.by, e.at));
        }
        Some(line)
    }
}

/// Why `next` handed nothing to an agent that asked.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct QueueRefusal {
    /// `at_limit` or `no_seat`.
    pub reason: String,
    pub limit: i64,
    /// The items the name holds, as `Q` numbers.
    pub held: Vec<String>,
    /// What to do about it.
    pub message: String,
}

/// The outcome of `next`: an item, a refusal, or neither when nothing is ready.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct QueueNext {
    pub claimed: Option<QueueItem>,
    pub refused: Option<QueueRefusal>,
}

/// An item an agent's name holds, for [`QueueOffer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldClaim {
    /// The item's `Q` number.
    pub label: String,
    /// The session the claim is bound to, if any.
    pub session: Option<String>,
}

/// What `next` would do for an agent now, read without writing ([`Store::queue_offer`]).
/// The hook packs and `queue done` and `queue release` build their queue lines from it.
#[derive(Debug, Clone)]
pub struct QueueOffer {
    /// The agent's address as the store resolves it.
    pub agent: String,
    /// The owner gave this name a seat: its own limit, above 0. A name on the default
    /// limit is not seated.
    pub seated: bool,
    /// The limit as `next` computes it: the name's own, else the default.
    pub limit: i64,
    /// The name's claimed items, oldest claim first.
    pub held: Vec<HeldClaim>,
    /// The ready mark of the item `next` would hand over now: `<item id>:<t>`, where `t`
    /// is when the item last came back to ready, or 0. `None` where `next` would refuse
    /// or find nothing. It identifies one offer and is never shown.
    pub ready: Option<String>,
}

impl QueueOffer {
    /// The ready mark, only for a seated name. Every caller that tells an agent that an
    /// item is ready goes through this, so a name on the default limit is told nothing.
    pub fn announce(&self) -> Option<&str> {
        self.ready.as_deref().filter(|_| self.seated)
    }
}

/// What a session keeps of a ready mark it was told about: the first 16 hex characters of
/// its SHA-256.
pub fn queue_stamp(mark: &str) -> String {
    let mut hex = crate::crypto::sha256_hex(mark.as_bytes());
    hex.truncate(16);
    hex
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct QueueLimit {
    pub agent: String,
    pub limit: i64,
    pub set_at: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct QueueLimits {
    /// The limit of an agent with no entry below.
    pub default_limit: i64,
    pub agents: Vec<QueueLimit>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct QueueDelegate {
    pub agent: String,
    pub added_at: String,
}

/// A new delegation. `key` is shown once; only its hash is stored.
#[derive(Debug, Clone, Serialize)]
pub struct NewQueueDelegate {
    pub agent: String,
    pub key: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct QueueGrace {
    pub grace_seconds: i64,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct QuietClaim {
    pub item: QueueItem,
    /// How long since the last sign of life from the claimer's name.
    pub quiet_for_seconds: i64,
    pub grace_ends_at: String,
    /// The claim is tied to no session: no session's end releases it.
    pub unbound: bool,
    /// When the session the claim is bound to was last seen by a hook.
    pub session_last_seen_at: Option<String>,
    /// Why the claim is listed, the first that holds: `name_quiet` (nothing from the
    /// claimer's name for an hour), `session_quiet` (the name is active and the claim's
    /// own session has been quiet for an hour) or `old_unbound` (the name is active and
    /// the claim, tied to no session, is at least one grace period old).
    pub reason: String,
    /// How long since a hook of the claim's session ran; `None` when it has no session.
    pub session_quiet_for_seconds: Option<i64>,
    /// How long ago the claim was made.
    #[serde(skip)]
    pub claimed_for_seconds: i64,
}

/// One claimed item and the session its claim is bound to: see [`Store::queue_claims`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueClaimed {
    /// The item's `Q` number.
    pub item: String,
    /// The claimer's address.
    pub agent: String,
    /// The session the claim is bound to; `None` when it is bound to none, or when that
    /// session's row no longer exists.
    pub session: Option<String>,
}

/// What one run of the queue sweep did.
#[derive(Debug, Clone, Default, Serialize, schemars::JsonSchema)]
pub struct QueueSweepReport {
    /// The items whose claim lapsed after the grace period, as `Q` numbers.
    pub released_claims: Vec<String>,
    /// Session rows deleted because no hook had reported them for 30 days.
    pub forgotten_sessions: u32,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct FailedNotice {
    /// The item's `Q` number.
    pub item: String,
    pub to: String,
    pub at: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SelfAdmitted {
    /// Self-admitted in the last seven days, newest first.
    pub recent: Vec<QueueItem>,
    /// Every self-admitted item; `another queue list --self-admitted` lists them.
    pub total: u32,
}

/// What needs the owner, one list per section, in the order they are shown.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct QueueNeedsOwner {
    /// Proposed security and data-loss items, oldest first.
    pub decide_now: Vec<QueueItem>,
    /// Other proposed items, oldest first.
    pub awaiting: Vec<QueueItem>,
    /// Claims with no sign of life for an hour or more.
    pub quiet_claims: Vec<QuietClaim>,
    /// Ready items no agent with a seat can take, or reserved for an agent not seen for an
    /// hour.
    pub nobody_can_take: Vec<QueueItem>,
    /// Notices that could not be sent in the last 24 hours.
    pub failed_notices: Vec<FailedNotice>,
    pub self_admitted: SelfAdmitted,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct AuditFinding {
    /// `duplicate`, `loss`, `admission` or `limit`.
    pub kind: String,
    /// The item's `Q` number, or its id when the log gives no number.
    pub item: String,
    pub at: String,
    pub detail: String,
}

/// What [`Store::queue_audit`] found in this node's `queue.*` events.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct QueueAudit {
    /// No finding.
    pub ok: bool,
    pub findings: Vec<AuditFinding>,
    pub items: u32,
    pub claims: u32,
    /// Releases by reason; handoffs are counted as `handoff`.
    pub releases: BTreeMap<String, u32>,
    pub completions: u32,
    /// Distinct agents that claimed an item.
    pub claimers: u32,
}

/// One `queue_items` row.
struct ItemRow {
    seq: i64,
    item_id: String,
    node_id: String,
    state: QueueState,
    kind: QueueKind,
    title: String,
    body: String,
    scope: Option<String>,
    done_condition: Option<String>,
    priority: QueuePriority,
    for_agent: Option<String>,
    skip_agent: Option<String>,
    proposed_by: Option<String>,
    proposed_at: i64,
    admit_authority: Option<String>,
    admitted_by: Option<String>,
    admitted_at: Option<i64>,
    claimer_id: Option<String>,
    claimed_at: Option<i64>,
    claim_session: Option<String>,
    outcome: Option<String>,
    result_summary: Option<String>,
    result_kind: Option<String>,
    result_ref: Option<String>,
    closed_by: Option<String>,
    closed_at: Option<i64>,
}

impl ItemRow {
    fn label(&self) -> String {
        label(self.seq)
    }
}

fn label(seq: i64) -> String {
    format!("Q{seq}")
}

const ITEM_COLUMNS: &str = "seq, item_id, node_id, state, kind, title, body, scope, \
     done_condition, priority, for_agent, skip_agent, proposed_by, proposed_at, admit_authority, \
     admitted_by, admitted_at, claimer_id, claimed_at, claim_session, outcome, result_summary, \
     result_kind, result_ref, closed_by, closed_at";

fn row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<ItemRow> {
    let state: String = r.get(3)?;
    let kind: String = r.get(4)?;
    Ok(ItemRow {
        seq: r.get(0)?,
        item_id: r.get(1)?,
        node_id: r.get(2)?,
        state: state.parse().unwrap_or(QueueState::Proposed),
        kind: kind.parse().unwrap_or_default(),
        title: r.get(5)?,
        body: r.get(6)?,
        scope: r.get(7)?,
        done_condition: r.get(8)?,
        priority: QueuePriority::of_rank(r.get(9)?),
        for_agent: r.get(10)?,
        skip_agent: r.get(11)?,
        proposed_by: r.get(12)?,
        proposed_at: r.get(13)?,
        admit_authority: r.get(14)?,
        admitted_by: r.get(15)?,
        admitted_at: r.get(16)?,
        claimer_id: r.get(17)?,
        claimed_at: r.get(18)?,
        claim_session: r.get(19)?,
        outcome: r.get(20)?,
        result_summary: r.get(21)?,
        result_kind: r.get(22)?,
        result_ref: r.get(23)?,
        closed_by: r.get(24)?,
        closed_at: r.get(25)?,
    })
}

/// Find an item on this node by its `Q` number (`Q12`, `q12`) or its id.
fn find_item(conn: &Connection, node_id: &str, raw: &str) -> Result<ItemRow> {
    let raw = raw.trim();
    let number = raw
        .strip_prefix(['Q', 'q'])
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|n| n.parse::<i64>().ok());
    let found = match number {
        Some(seq) => conn
            .query_row(
                &format!("SELECT {ITEM_COLUMNS} FROM queue_items WHERE seq = ?1 AND node_id = ?2"),
                params![seq, node_id],
                row_from,
            )
            .optional()?,
        None => conn
            .query_row(
                &format!(
                    "SELECT {ITEM_COLUMNS} FROM queue_items WHERE item_id = ?1 AND node_id = ?2"
                ),
                params![raw.to_ascii_uppercase(), node_id],
                row_from,
            )
            .optional()?,
    };
    found.ok_or_else(|| Error::NotFound(format!("unknown queue item {raw:?}")))
}

fn address_of(conn: &Connection, agent_id: &str) -> Result<String> {
    Ok(conn
        .query_row(
            "SELECT coalesce(address, name) FROM agents WHERE agent_id = ?1",
            [agent_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_else(|| agent_id.to_string()))
}

fn address_opt(conn: &Connection, agent_id: Option<&str>) -> Result<Option<String>> {
    agent_id.map(|id| address_of(conn, id)).transpose()
}

fn history_of(conn: &Connection, item_id: &str) -> Result<Vec<QueueHistoryEntry>> {
    let mut stmt = conn.prepare(
        "SELECT at, kind, actor, note, detail FROM queue_history WHERE item_id = ?1
         ORDER BY rowid",
    )?;
    let rows = stmt.query_map([item_id], |r| {
        let detail: Option<String> = r.get(4)?;
        Ok(QueueHistoryEntry {
            at: fmt_ts(r.get(0)?),
            kind: r.get(1)?,
            actor: r.get(2)?,
            note: r.get(3)?,
            detail: detail.and_then(|d| serde_json::from_str(&d).ok()),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The last edit of the scope or done condition recorded after the item's admission.
fn edited_after(conn: &Connection, item_id: &str) -> Result<Option<QueueEdited>> {
    let mut stmt = conn.prepare(
        "SELECT actor, at, detail FROM queue_history
         WHERE item_id = ?1 AND kind = 'queue.updated'
           AND rowid > (SELECT max(rowid) FROM queue_history
                        WHERE item_id = ?1 AND kind = 'queue.admitted')
         ORDER BY rowid DESC",
    )?;
    let rows = stmt
        .query_map([item_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (actor, at, detail) in rows {
        let detail: Value = detail
            .and_then(|d| serde_json::from_str(&d).ok())
            .unwrap_or(Value::Null);
        if detail.get("scope").is_some() || detail.get("done_condition").is_some() {
            return Ok(Some(QueueEdited {
                by: actor,
                at: fmt_ts(at),
            }));
        }
    }
    Ok(None)
}

fn to_item(conn: &Connection, row: ItemRow, with_history: bool) -> Result<QueueItem> {
    let admitted = match (&row.admit_authority, row.admitted_at) {
        (Some(authority), Some(at)) => Some(QueueAdmitted {
            authority: authority.clone(),
            by: match &row.admitted_by {
                Some(id) => address_of(conn, id)?,
                None => "owner".to_string(),
            },
            at: fmt_ts(at),
            self_admitted: authority == "delegate" && row.admitted_by == row.proposed_by,
            edited_after: edited_after(conn, &row.item_id)?,
        }),
        _ => None,
    };
    let claim = match (&row.claimer_id, row.claimed_at) {
        (Some(id), Some(at)) => Some(QueueClaim {
            agent: address_of(conn, id)?,
            at: fmt_ts(at),
            session_bound: session_seen(conn, &row)?.is_some(),
        }),
        _ => None,
    };
    let result = match (&row.outcome, row.closed_at) {
        (Some(outcome), Some(at)) => Some(QueueResult {
            outcome: outcome.clone(),
            summary: row.result_summary.clone(),
            pointer_kind: row.result_kind.clone(),
            pointer: row.result_ref.clone(),
            by: match &row.closed_by {
                Some(id) => address_of(conn, id)?,
                None => "owner".to_string(),
            },
            at: fmt_ts(at),
        }),
        _ => None,
    };
    let history = if with_history {
        Some(history_of(conn, &row.item_id)?)
    } else {
        None
    };
    Ok(QueueItem {
        label: row.label(),
        for_agent: address_opt(conn, row.for_agent.as_deref())?,
        skip_agent: address_opt(conn, row.skip_agent.as_deref())?,
        proposed_by: match &row.proposed_by {
            Some(id) => address_of(conn, id)?,
            None => "owner".to_string(),
        },
        proposed_at: fmt_ts(row.proposed_at),
        admitted,
        claim,
        result,
        history,
        item_id: row.item_id,
        seq: row.seq,
        node_id: row.node_id,
        state: row.state,
        kind: row.kind,
        title: row.title,
        body: row.body,
        scope: row.scope,
        done_condition: row.done_condition,
        priority: row.priority,
    })
}

/// When a hook last reported the session a claim is bound to. `None` when the item is not
/// claimed, the claim names no session, or that session's row no longer exists: such a
/// claim is bound to nothing, because no session's end will release it.
fn session_seen(conn: &Connection, row: &ItemRow) -> Result<Option<i64>> {
    let (Some(claimer), Some(session)) = (&row.claimer_id, &row.claim_session) else {
        return Ok(None);
    };
    Ok(conn
        .query_row(
            "SELECT seen_at FROM queue_sessions WHERE agent_id = ?1 AND session_id = ?2",
            params![claimer, session],
            |r| r.get(0),
        )
        .optional()?)
}

/// The item as it now is, with its history.
fn item_now(conn: &Connection, node_id: &str, item_id: &str) -> Result<QueueItem> {
    to_item(conn, find_item(conn, node_id, item_id)?, true)
}

/// Refuse text a local caller wrote that is over `max` bytes. The text goes into an event
/// that is synced to peers, so it stays small (D99); the message follows the resource caps.
pub(crate) fn within_bytes(what: &str, text: &str, max: usize) -> Result<()> {
    let len = text.len();
    if len > max {
        return Err(Error::Invalid(format!(
            "{what} is {len} bytes; the limit is {max}"
        )));
    }
    Ok(())
}

pub(crate) fn no_unsafe(what: &str, text: &str, multi_line: bool) -> Result<()> {
    // The Unicode line and paragraph separators end a line for some readers, so
    // one-line text refuses them as it refuses a line feed. Multi-line text
    // allows them, the same way it allows a line feed and a tab.
    let bad = |c: char| {
        is_unsafe_text_char(c)
            && !(multi_line && matches!(c, '\n' | '\t' | '\u{2028}' | '\u{2029}'))
    };
    if !text.chars().any(bad) {
        return Ok(());
    }
    // The message names what was found: a text-direction character is not a control.
    if !text
        .chars()
        .any(|c| bad(c) && (c.is_control() || matches!(c, '\u{2028}' | '\u{2029}')))
    {
        return Err(Error::Invalid(format!(
            "{what} holds a text-direction character (U+202A to U+202E or U+2066 to U+2069), which changes the order text is shown in: remove it"
        )));
    }
    if multi_line && text.contains('\r') {
        return Err(Error::Invalid(format!(
            "{what} contains a carriage return: end lines with a line feed only (convert CRLF line endings first)"
        )));
    }
    Err(Error::Invalid(if multi_line {
        format!("{what} must be plain text: line breaks and tabs are allowed, escape and other control characters are not")
    } else {
        format!("{what} must be one line of text: no line breaks, tabs, escape or other control characters")
    }))
}

/// Check a text that must not be empty: `max` characters, on one line or several.
fn check_text(what: &str, text: &str, max: usize, multi_line: bool) -> Result<()> {
    if text.trim().is_empty() {
        return Err(Error::Invalid(format!("{what} must not be empty")));
    }
    no_unsafe(what, text, multi_line)?;
    if text.chars().count() > max {
        return Err(Error::Invalid(format!(
            "{what} is longer than {max} characters"
        )));
    }
    Ok(())
}

fn check_scope(scope: &str) -> Result<()> {
    let ok = !scope.is_empty()
        && scope.chars().count() <= MAX_SCOPE_CHARS
        && scope
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "the scope id must be 1 to {MAX_SCOPE_CHARS} characters of letters, digits, '.', '_' or '-', for example M1"
        )))
    }
}

fn check_done_condition(text: &str) -> Result<()> {
    check_text("the done condition", text, MAX_DONE_CONDITION_CHARS, true)
}

fn check_pointer(kind: PointerKind, pointer: &str) -> Result<()> {
    no_unsafe("the result pointer", pointer, false)?;
    let ok = match kind {
        PointerKind::Pr => {
            let number = pointer.strip_prefix('#').unwrap_or(pointer);
            let is_number = !number.is_empty()
                && number.len() <= 18
                && number.bytes().all(|b| b.is_ascii_digit());
            let is_url = pointer.len() > "https://".len()
                && pointer.starts_with("https://")
                && pointer.chars().count() <= MAX_POINTER_CHARS
                && !pointer.chars().any(char::is_whitespace);
            is_number || is_url
        }
        PointerKind::Commit => {
            (7..=64).contains(&pointer.len()) && pointer.bytes().all(|b| b.is_ascii_hexdigit())
        }
        PointerKind::File => {
            !pointer.trim().is_empty() && pointer.chars().count() <= MAX_POINTER_CHARS
        }
    };
    if ok {
        return Ok(());
    }
    Err(Error::Invalid(match kind {
        PointerKind::Pr => {
            "--pr takes a pull request number (12 or #12) or an https URL".to_string()
        }
        PointerKind::Commit => "--commit takes 7 to 64 hex characters".to_string(),
        PointerKind::File => {
            format!("--file takes a path of 1 to {MAX_POINTER_CHARS} characters on one line")
        }
    }))
}

impl ProposeItem {
    /// Check the texts against the queue's rules, before anything is read or written.
    pub fn validate(&self) -> Result<()> {
        check_text("the title", &self.title, MAX_TITLE_CHARS, false)?;
        no_unsafe("the body", &self.body, true)?;
        if self.body.len() > MAX_BODY_BYTES {
            return Err(Error::Invalid(format!(
                "the body is larger than {MAX_BODY_BYTES} bytes"
            )));
        }
        check_scope_and_done(self.scope.as_deref(), self.done_when.as_deref())
    }
}

impl AdmitItem {
    /// Check the texts against the queue's rules, before anything is read or written.
    pub fn validate(&self) -> Result<()> {
        check_scope_and_done(self.scope.as_deref(), self.done_when.as_deref())
    }
}

impl EditItem {
    /// Check the texts, and that the edit asks for one change at least.
    pub fn validate(&self) -> Result<()> {
        check_scope_and_done(self.scope.as_deref(), self.done_when.as_deref())?;
        if self.for_agent.is_some() && self.for_anyone {
            return Err(Error::Invalid(
                "pass --for AGENT or --for-anyone, not both".into(),
            ));
        }
        let nothing = self.scope.is_none()
            && self.done_when.is_none()
            && self.priority.is_none()
            && self.for_agent.is_none()
            && !self.for_anyone
            && self.kind.is_none();
        if nothing {
            return Err(Error::Invalid(
                "nothing to change: pass --scope, --done-when, --priority, --for, --for-anyone or --kind".into(),
            ));
        }
        Ok(())
    }
}

impl ReleaseItem {
    /// Check the note and the reason, and that `to` and `reason` are not both given.
    pub fn validate(&self) -> Result<()> {
        if let Some(note) = &self.note {
            check_text("the note", note, MAX_NOTE_CHARS, true)?;
        }
        if let Some(reason) = &self.reason {
            QueueItem::validate_reason(reason)?;
        }
        if self.to.is_some() && self.reason.is_some() {
            return Err(Error::Invalid(
                "pass --to (the claimer hands the item on) or --reason (an admitter releases it), not both".into(),
            ));
        }
        Ok(())
    }
}

impl QueueItem {
    /// Check an admitter's reason for rejecting an item or releasing a claim.
    pub fn validate_reason(reason: &str) -> Result<()> {
        check_text("the reason", reason, MAX_REASON_CHARS, true)
    }
}

impl QueueLimits {
    /// Check a limit: 0 to 20.
    pub fn validate(limit: i64) -> Result<()> {
        check_limit(limit)
    }
}

impl QueueGrace {
    /// Check a grace period in seconds: 2 hours to 30 days.
    pub fn validate(seconds: i64) -> Result<()> {
        if (MIN_QUEUE_GRACE_SECONDS..=MAX_QUEUE_GRACE_SECONDS).contains(&seconds) {
            Ok(())
        } else {
            Err(Error::Invalid(
                "the grace period must be between 2 hours and 30 days".into(),
            ))
        }
    }
}

fn check_scope_and_done(scope: Option<&str>, done_when: Option<&str>) -> Result<()> {
    if let Some(scope) = scope {
        check_scope(scope)?;
    }
    if let Some(done_when) = done_when {
        check_done_condition(done_when)?;
    }
    Ok(())
}

/// What changes whenever an item does: the id of its last history row. Every transition,
/// note and edit that changes anything writes one.
fn revision_of(conn: &Connection, item_id: &str) -> Result<String> {
    Ok(conn
        .query_row(
            "SELECT event_id FROM queue_history WHERE item_id = ?1 ORDER BY rowid DESC LIMIT 1",
            [item_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_default())
}

/// Refuse a write whose caller decided on an earlier state of the item than the one it
/// would change. `shown` is the revision the caller was shown, if it was shown one.
fn unchanged_since(conn: &Connection, row: &ItemRow, shown: Option<&str>) -> Result<()> {
    match shown {
        Some(revision) if revision_of(conn, &row.item_id)? != revision => {
            Err(Error::Conflict(format!(
                "{label} changed while you were deciding; nothing was done. Look again with another queue show {label}, then run the command again",
                label = row.label()
            )))
        }
        _ => Ok(()),
    }
}

/// Whether the item's skip still holds: it ends one hour after the release or handoff that
/// set it, so a node with one agent is not left with a ready item nobody can take.
fn skip_holds(conn: &Connection, row: &ItemRow, now: i64) -> Result<bool> {
    if row.skip_agent.is_none() {
        return Ok(false);
    }
    let given_up = given_up_at(conn, &row.item_id)?;
    Ok(given_up.is_none_or(|at| now.saturating_sub(at) < SKIP_MS))
}

const GIVEN_UP_SQL: &str = "SELECT max(at) FROM queue_history
     WHERE item_id = ?1 AND kind IN ('queue.released', 'queue.handoff')";

/// When the item last came back to ready: its latest release or handoff.
fn given_up_at(conn: &Connection, item_id: &str) -> Result<Option<i64>> {
    Ok(conn.query_row(GIVEN_UP_SQL, [item_id], |r| r.get(0))?)
}

fn setting(conn: &Connection, key: &str, default: i64) -> Result<i64> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM queue_settings WHERE key = ?1",
            [key],
            |r| r.get(0),
        )
        .optional()?;
    Ok(value.and_then(|v| v.parse().ok()).unwrap_or(default))
}

fn default_limit(conn: &Connection) -> Result<i64> {
    setting(conn, "default_limit", DEFAULT_QUEUE_LIMIT)
}

fn grace_seconds(conn: &Connection) -> Result<i64> {
    setting(conn, "grace_seconds", DEFAULT_QUEUE_GRACE_SECONDS)
}

const OWN_LIMIT_SQL: &str = "SELECT max_claimed FROM queue_limits WHERE agent_id = ?1";
const HELD_SQL: &str = "SELECT seq, claim_session FROM queue_items
     WHERE claimer_id = ?1 AND state = 'claimed' ORDER BY claimed_at, seq";

/// Where an agent stands with the queue before the ready rows are looked at. `next` and
/// the offer both read it here, so they cannot disagree on a seat or a limit.
struct Standing {
    /// The limit the owner set for this name, if any.
    own_limit: Option<i64>,
    /// The name's own limit, else the default.
    limit: i64,
    /// The name's claimed items, oldest claim first: `seq` and the bound session.
    held: Vec<(i64, Option<String>)>,
}

impl Standing {
    /// Whether `next` would look for a ready row at all.
    fn has_room(&self) -> bool {
        self.limit > 0 && (self.held.len() as i64) < self.limit
    }

    fn labels(&self) -> Vec<String> {
        self.held.iter().map(|(seq, _)| label(*seq)).collect()
    }
}

/// The limit the owner set for this name, if any.
fn own_limit(conn: &Connection, agent_id: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(OWN_LIMIT_SQL, [agent_id], |r| r.get(0))
        .optional()?)
}

fn limit_of(conn: &Connection, agent_id: &str) -> Result<i64> {
    match own_limit(conn, agent_id)? {
        Some(n) => Ok(n),
        None => default_limit(conn),
    }
}

fn standing(conn: &Connection, agent_id: &str) -> Result<Standing> {
    let own_limit = own_limit(conn, agent_id)?;
    let limit = match own_limit {
        Some(n) => n,
        None => default_limit(conn)?,
    };
    let mut stmt = conn.prepare(HELD_SQL)?;
    let held = stmt
        .query_map([agent_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Standing {
        own_limit,
        limit,
        held,
    })
}

fn ready_sql() -> String {
    format!(
        "SELECT {ITEM_COLUMNS} FROM queue_items
         WHERE state = 'ready' AND node_id = ?1
           AND (for_agent IS NULL OR for_agent = ?2)
           AND (skip_agent IS NULL OR skip_agent != ?2
                OR (SELECT max(h.at) FROM queue_history h
                    WHERE h.item_id = queue_items.item_id
                      AND h.kind IN ('queue.released', 'queue.handoff')) <= ?3)
         ORDER BY priority, admitted_at, seq LIMIT 1"
    )
}

/// The ready item `next` hands this agent: the only copy of that selection. It honours
/// reservations, the one-hour skip and this node only, in the order priority, admission
/// time, sequence.
fn first_ready(
    conn: &Connection,
    node_id: &str,
    agent_id: &str,
    now: i64,
) -> Result<Option<ItemRow>> {
    Ok(conn
        .query_row(
            &ready_sql(),
            params![node_id, agent_id, now - SKIP_MS],
            row_from,
        )
        .optional()?)
}

fn set_setting_tx(tx: &Tx<'_>, key: &str, value: i64, event_id: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO queue_settings (key, value, set_at, event_id) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(key) DO UPDATE SET value = ?2, set_at = ?3, event_id = ?4",
        params![key, value.to_string(), tx.now, event_id],
    )?;
    Ok(())
}

/// The authority an admitter acts with.
enum Authority {
    Owner,
    Delegate(AgentRef),
}

impl Authority {
    fn name(&self) -> &'static str {
        match self {
            Authority::Owner => "owner",
            Authority::Delegate(_) => "delegate",
        }
    }

    /// How the authority was shown: the typed yes at a terminal, or the key.
    fn via(&self) -> &'static str {
        match self {
            Authority::Owner => "terminal",
            Authority::Delegate(_) => "key",
        }
    }

    fn actor(&self) -> &str {
        match self {
            Authority::Owner => "owner",
            Authority::Delegate(a) => &a.address,
        }
    }

    fn agent_id(&self) -> Option<&str> {
        match self {
            Authority::Owner => None,
            Authority::Delegate(a) => Some(&a.id),
        }
    }

    /// A delegate decides `work` items only.
    fn may_decide(&self, kind: QueueKind) -> Result<()> {
        match self {
            Authority::Delegate(_) if kind.is_fast_lane() => {
                Err(Error::Forbidden(FAST_LANE_IS_THE_OWNERS.into()))
            }
            _ => Ok(()),
        }
    }
}

fn is_delegate_key(conn: &Connection, agent_id: &str, key: &str) -> Result<bool> {
    let stored: Option<String> = conn
        .query_row(
            "SELECT key_hash FROM queue_delegates WHERE agent_id = ?1 AND removed_at IS NULL",
            [agent_id],
            |r| r.get(0),
        )
        .optional()?;
    let digest = crate::crypto::sha256_hex(key.as_bytes());
    Ok(stored.is_some_and(|h| crate::crypto::secrets_eq(&h, &digest)))
}

/// The caller as an admitter: the owner, or a delegate that presented its key.
fn admitter(conn: &Connection, node_id: &str, caller: &QueueCaller) -> Result<Authority> {
    match caller {
        QueueCaller::Owner => Ok(Authority::Owner),
        QueueCaller::Agent { agent, admit_key } => {
            let me = resolve_agent(conn, node_id, agent)?;
            match admit_key {
                Some(key) if is_delegate_key(conn, &me.id, key)? => Ok(Authority::Delegate(me)),
                Some(_) => Err(Error::Forbidden(format!(
                    "missing or wrong delegate key for {}: the owner gives one with another queue delegate add {} --owner",
                    me.address, me.address
                ))),
                None => Err(Error::Forbidden(format!(
                    "{} is not an admitter: only the owner (--owner at a terminal) or a delegate with its key in ANOTHER_ADMIT_KEY decides this",
                    me.address
                ))),
            }
        }
    }
}

/// Refuse anyone but the owner. A delegate key does not cover an owner-only action.
fn owner_only(caller: &QueueCaller, what: &str) -> Result<()> {
    match caller {
        QueueCaller::Owner => Ok(()),
        QueueCaller::Agent { .. } => Err(Error::Forbidden(format!(
            "{what} is the owner's alone: run the command with --owner at a terminal; a delegate key does not cover it"
        ))),
    }
}

/// The item's state in words, with its claimer.
fn state_words(conn: &Connection, row: &ItemRow) -> Result<String> {
    Ok(match (row.state, &row.claimer_id, &row.outcome) {
        (QueueState::Claimed, Some(id), _) => format!("claimed by {}", address_of(conn, id)?),
        (QueueState::Done, _, Some(outcome)) => format!("done ({outcome})"),
        (state, _, _) => state.as_str().to_string(),
    })
}

fn wrong_state(conn: &Connection, row: &ItemRow, hint: &str) -> Result<Error> {
    Ok(Error::Conflict(format!(
        "{} is {}; {hint}",
        row.label(),
        state_words(conn, row)?
    )))
}

/// The agent a `--for` or `--to` names: another local agent with a seat.
fn named_agent(
    conn: &Connection,
    node_id: &str,
    raw: &str,
    caller_id: Option<&str>,
    flag: &str,
) -> Result<AgentRef> {
    let named = resolve_any(conn, node_id, raw)?;
    if named.node_id != node_id {
        return Err(Error::Invalid(format!(
            "{flag} must name an agent on this node; {} is on another node",
            named.address
        )));
    }
    if caller_id == Some(named.id.as_str()) {
        return Err(Error::Invalid(format!(
            "{flag} must name another agent, not {}",
            named.address
        )));
    }
    if limit_of(conn, &named.id)? == 0 {
        return Err(Error::Invalid(format!(
            "{flag} names {}, whose limit is 0: it has no seat in the queue",
            named.address
        )));
    }
    Ok(named)
}

/// Append an item event and its history row. Every item event carries the item's id, its
/// home node and its number.
fn record_tx(
    tx: &Tx<'_>,
    row: &ItemRow,
    actor: &str,
    kind: &str,
    extra: Value,
    note: Option<&str>,
    detail: Option<Value>,
) -> Result<String> {
    let mut payload = Map::new();
    payload.insert("item_id".into(), json!(row.item_id));
    payload.insert("node_id".into(), json!(row.node_id));
    payload.insert("seq".into(), json!(row.seq));
    if let Value::Object(extra) = extra {
        payload.extend(extra);
    }
    let event_id = tx.emit(actor, kind, Value::Object(payload))?;
    tx.execute(
        "INSERT INTO queue_history (item_id, event_id, at, kind, actor, note, detail)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            row.item_id,
            event_id,
            tx.now,
            kind,
            actor,
            note,
            detail.map(|d| d.to_string())
        ],
    )?;
    Ok(event_id)
}

/// Mail a notice about an item in its own savepoint. A notice that cannot be sent is
/// recorded as `queue.notify_failed` and the caller's change stands.
fn notify_tx(
    tx: &Tx<'_>,
    row: &ItemRow,
    from_id: &str,
    to_id: &str,
    reason: &str,
    subject: String,
    body: String,
) -> Result<Option<String>> {
    let notice = SendMessage {
        subject,
        body,
        ..Default::default()
    };
    let sent = crate::holds::attempt(tx, "queue_notice", || {
        crate::mail::send_notice_tx(tx, from_id, to_id, &notice)
    });
    match sent {
        Ok(message) => Ok(Some(message.message_id)),
        Err(e) => {
            tx.emit(
                "system",
                "queue.notify_failed",
                json!({
                    "item_id": row.item_id,
                    "node_id": row.node_id,
                    "seq": row.seq,
                    "to": to_id,
                    "reason": reason,
                    "error": e.to_string(),
                }),
            )?;
            Ok(None)
        }
    }
}

/// Return a claim to `ready`, keeping the approval, the admission time and the priority.
pub(crate) fn actor_ended_tx(tx: &Tx<'_>, actor_id: &str) -> Result<()> {
    let mut stmt = tx.prepare(&format!(
        "SELECT {ITEM_COLUMNS} FROM queue_items WHERE claimer_id = ?1 AND state = 'claimed'"
    ))?;
    let rows = stmt
        .query_map([actor_id], row_from)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for row in rows {
        lapse_tx(
            tx,
            &row,
            "session_end",
            "the claiming session ended",
            json!({"actor_id": actor_id}),
        )?;
    }
    Ok(())
}

pub(crate) fn projection_ended_tx(tx: &Tx<'_>, item: &str) -> Result<()> {
    let row = find_item(tx, tx.node_id, item)?;
    if row.state == QueueState::Claimed {
        lapse_tx(
            tx,
            &row,
            "lease_ended",
            "the claim lease ended",
            json!({"item_id": item}),
        )?;
    }
    Ok(())
}

pub(crate) fn discard_legacy_claims_tx(tx: &Tx<'_>) -> Result<()> {
    let mut stmt = tx.prepare(&format!("SELECT {ITEM_COLUMNS} FROM queue_items WHERE state = 'claimed' AND NOT EXISTS (SELECT 1 FROM contract_actors a WHERE a.actor_id = queue_items.claimer_id)"))?;
    let rows = stmt
        .query_map([], row_from)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for row in rows {
        lapse_tx(
            tx,
            &row,
            "legacy_informational",
            "legacy labels are not actor responsibility",
            json!({"item_id":row.item_id}),
        )?;
    }
    tx.execute("UPDATE queue_items SET for_agent = NULL WHERE for_agent IS NOT NULL AND NOT EXISTS (SELECT 1 FROM contract_actors a WHERE a.actor_id = queue_items.for_agent)", [])?;
    tx.execute("UPDATE queue_items SET skip_agent = NULL WHERE skip_agent IS NOT NULL AND NOT EXISTS (SELECT 1 FROM contract_actors a WHERE a.actor_id = queue_items.skip_agent)", [])?;
    Ok(())
}

fn unclaim_tx(
    tx: &Tx<'_>,
    row: &ItemRow,
    for_agent: Option<&str>,
    skip_agent: Option<&str>,
) -> Result<()> {
    crate::actor_leases::projection_release(tx, &format!("queue:{}", row.item_id), "released")?;
    tx.execute(
        "UPDATE queue_items SET state = 'ready', claimer_id = NULL, claimed_at = NULL,
           claim_session = NULL, for_agent = ?2, skip_agent = ?3, updated_at = ?4
         WHERE item_id = ?1",
        params![row.item_id, for_agent, skip_agent, tx.now],
    )?;
    Ok(())
}

/// The session id a hook call carries, when its client is one whose ids are relied on. An
/// id over [`MAX_SESSION_ID_BYTES`] is no id: nothing is stored or bound under it.
fn hook_session(info: &SessionInfo) -> Option<&str> {
    let client = info.client.as_deref()?;
    if !QUEUE_SESSION_CLIENTS.contains(&client) {
        return None;
    }
    info.session_id
        .as_deref()
        .filter(|id| !id.is_empty() && id.len() <= MAX_SESSION_ID_BYTES)
}

/// A local hook of a session reported in: make or refresh the session's row. A row that
/// had ended is open again, since a resumed session may reuse its id.
pub(crate) fn session_seen_tx(tx: &Tx<'_>, agent_id: &str, info: &SessionInfo) -> Result<()> {
    let Some(session) = hook_session(info) else {
        return Ok(());
    };
    tx.execute(
        "INSERT INTO queue_sessions (agent_id, session_id, started_at, seen_at, ended_at)
         VALUES (?1, ?2, ?3, ?3, NULL)
         ON CONFLICT(agent_id, session_id) DO UPDATE SET seen_at = ?3, ended_at = NULL",
        params![agent_id, session, tx.now],
    )?;
    Ok(())
}

/// The end hook of a session ran: close its row and return every item claimed by that
/// session to `ready`. `ended_event` is the `session.ended` event. An end that names no
/// session, or a session with no row, releases nothing.
pub(crate) fn session_ended_tx(
    tx: &Tx<'_>,
    agent_id: &str,
    info: &SessionInfo,
    ended_event: &str,
) -> Result<()> {
    let Some(session) = hook_session(info) else {
        return Ok(());
    };
    let closed = tx.execute(
        "UPDATE queue_sessions SET ended_at = ?3 WHERE agent_id = ?1 AND session_id = ?2",
        params![agent_id, session, tx.now],
    )?;
    if closed == 0 {
        return Ok(());
    }
    let mut stmt = tx.prepare(&format!(
        "SELECT {ITEM_COLUMNS} FROM queue_items
         WHERE claimer_id = ?1 AND state = 'claimed' AND claim_session = ?2 AND node_id = ?3
         ORDER BY claimed_at, seq"
    ))?;
    let rows = stmt
        .query_map(params![agent_id, session, tx.node_id], row_from)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for row in rows {
        lapse_tx(
            tx,
            &row,
            "session_end",
            "the session that claimed it ended",
            json!({ "session_id": session, "event_id": ended_event }),
        )?;
    }
    Ok(())
}

/// Return a claim to `ready` without the claimer or an admitter asking, and mail the
/// former claimer. A lapse sets no skip, and ends a reservation for the former claimer, so
/// another agent can take the item and the agent that comes back can too.
fn lapse_tx(tx: &Tx<'_>, row: &ItemRow, reason: &str, words: &str, evidence: Value) -> Result<()> {
    let (Some(former), Some(claimed_at)) = (row.claimer_id.as_deref(), row.claimed_at) else {
        return Ok(());
    };
    let cleared = row.for_agent.as_deref().filter(|f| *f == former);
    let for_agent = match cleared {
        Some(_) => None,
        None => row.for_agent.as_deref(),
    };
    unclaim_tx(tx, row, for_agent, None)?;
    // The system has no agent: the notice comes from the former claimer's own address.
    let notice = notify_tx(
        tx,
        row,
        former,
        former,
        reason,
        format!("Claim released: {}", row.label()),
        format!(
            "Your claim on {label} was released at {}: {words}. The claim and its notes are kept in the item's history. If you are still working on it, run another queue next and check that {label} comes back; another agent may take it first. {AUTOMATIC}",
            fmt_ts(tx.now),
            label = row.label()
        ),
    )?;
    record_tx(
        tx,
        row,
        "system",
        "queue.released",
        json!({
            "reason": reason,
            "previous_claimer_id": former,
            "claimed_at": claimed_at,
            "note": null,
            "evidence": evidence,
            "for_agent_cleared": cleared,
            "notice_message_id": notice,
        }),
        None,
        Some(json!({
            "reason": reason,
            "evidence": evidence,
            "for_agent_cleared": cleared,
        })),
    )?;
    Ok(())
}

/// The queue sweep: release every claim with no sign of life for the grace period, and
/// delete session rows no hook has reported for 30 days. It never releases a claim held by
/// `calling_agent`, the agent whose own call runs it. The last sign of life of a claim is
/// the later of its claim time and the claimer's `last_seen_at`.
///
/// Not part of [`crate::holds::sweep_tx`], which also runs at the start of reads, before
/// any sign of life of the reader.
pub(crate) fn queue_sweep_tx(tx: &Tx<'_>, calling_agent: Option<&str>) -> Result<QueueSweepReport> {
    let grace = grace_seconds(tx)?;
    let grace_ms = grace.saturating_mul(1000);
    let mut stmt = tx.prepare(&format!(
        "SELECT {ITEM_COLUMNS} FROM queue_items
         WHERE state = 'claimed' AND claimer_id IS NOT NULL AND node_id = ?1
           AND (?2 IS NULL OR claimer_id != ?2)
         ORDER BY claimed_at, seq"
    ))?;
    let claimed = stmt
        .query_map(params![tx.node_id, calling_agent], row_from)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut released_claims = Vec::new();
    for row in claimed {
        if tx
            .query_row(
                "SELECT 1 FROM lease_projections WHERE projection_id = ?1",
                [format!("queue:{}", row.item_id)],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            continue;
        }
        let (Some(claimer), Some(claimed_at)) = (&row.claimer_id, row.claimed_at) else {
            continue;
        };
        let last_sign = claimed_at.max(last_seen(tx, claimer)?.unwrap_or(0));
        if tx.now.saturating_sub(last_sign) < grace_ms {
            continue;
        }
        lapse_tx(
            tx,
            &row,
            "grace",
            "nothing was heard from you for the grace period",
            json!({ "last_sign_of_life": last_sign, "grace_seconds": grace }),
        )?;
        released_claims.push(row.label());
    }
    let forgotten_sessions = tx.execute(
        "DELETE FROM queue_sessions WHERE seen_at <= ?1",
        [tx.now - SESSION_ROW_KEPT_MS],
    )? as u32;
    Ok(QueueSweepReport {
        released_claims,
        forgotten_sessions,
    })
}

/// The session a new claim by this agent is bound to: the one session of the name that is
/// fresh (not ended, and seen within the grace period). With none, or with two or more,
/// the claim is bound to no session. A row not seen for the grace period cannot tell a
/// dead session from a silent one, so it does not count.
fn fresh_session(conn: &Connection, agent_id: &str, now: i64) -> Result<Option<String>> {
    let grace_ms = grace_seconds(conn)?.saturating_mul(1000);
    let mut stmt = conn.prepare(
        "SELECT session_id FROM queue_sessions
         WHERE agent_id = ?1 AND ended_at IS NULL AND seen_at > ?2 LIMIT 2",
    )?;
    let mut fresh = stmt
        .query_map(params![agent_id, now - grace_ms], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(match fresh.len() {
        1 => fresh.pop(),
        _ => None,
    })
}

impl Store {
    /// Run the queue sweep with no calling agent: see [`queue_sweep_tx`]. The serve
    /// sweeper and `another sweep` call it beside [`Store::sweep`].
    pub fn queue_sweep(&self) -> Result<QueueSweepReport> {
        self.write(|tx| queue_sweep_tx(tx, None))
    }

    /// Propose an item. Any local agent may, and so may the owner. Nothing is mailed.
    pub fn queue_propose(&self, caller: &QueueCaller, input: ProposeItem) -> Result<QueueItem> {
        input.validate()?;
        self.write(|tx| {
            let proposer = match caller {
                QueueCaller::Owner => None,
                QueueCaller::Agent { agent, .. } => Some(crate::identity::resolve_acting_agent(
                    tx, tx.node_id, agent,
                )?),
            };
            let actor = proposer.as_ref().map_or("owner", |a| a.address.as_str());
            let proposer_id = proposer.as_ref().map(|a| a.id.as_str());
            let item_id = new_id();
            tx.execute(
                "INSERT INTO queue_items (item_id, node_id, state, kind, title, body, scope,
                   done_condition, proposed_by, proposed_at, updated_at)
                 VALUES (?1, ?2, 'proposed', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)",
                params![
                    item_id,
                    tx.node_id,
                    input.kind.as_str(),
                    input.title,
                    input.body,
                    input.scope,
                    input.done_when,
                    proposer_id,
                    tx.now
                ],
            )?;
            let row = find_item(tx, tx.node_id, &item_id)?;
            record_tx(
                tx,
                &row,
                actor,
                "queue.proposed",
                json!({
                    "kind": input.kind.as_str(),
                    "title": input.title,
                    "body": input.body,
                    "scope": input.scope,
                    "done_condition": input.done_when,
                    "proposed_by": proposer_id,
                }),
                None,
                Some(json!({ "kind": input.kind.as_str() })),
            )?;
            if let Some(id) = proposer_id {
                touch(tx, id)?;
            }
            to_item(tx, row, true)
        })
    }

    /// Admit a proposed item: it becomes `ready` with its scope, its done condition and the
    /// record of who admitted it and when. Only the owner or a delegate may; a security or
    /// data-loss item is the owner's alone.
    pub fn queue_admit(
        &self,
        caller: &QueueCaller,
        item: &str,
        input: AdmitItem,
    ) -> Result<QueueItem> {
        self.admit(caller, item, input, None)
    }

    /// [`Store::queue_admit`] for a caller that was shown the item first: refused with
    /// `conflict` unless the item is still at `revision` (see [`Store::queue_item_shown`]).
    pub fn queue_admit_shown(
        &self,
        caller: &QueueCaller,
        item: &str,
        input: AdmitItem,
        revision: &str,
    ) -> Result<QueueItem> {
        self.admit(caller, item, input, Some(revision))
    }

    fn admit(
        &self,
        caller: &QueueCaller,
        item: &str,
        input: AdmitItem,
        shown: Option<&str>,
    ) -> Result<QueueItem> {
        input.validate()?;
        self.write(|tx| {
            let authority = admitter(tx, tx.node_id, caller)?;
            let row = find_item(tx, tx.node_id, item)?;
            unchanged_since(tx, &row, shown)?;
            authority.may_decide(row.kind)?;
            if row.state != QueueState::Proposed {
                return Err(wrong_state(tx, &row, "only a proposed item is admitted")?);
            }
            let missing = |what: &str, flag: &str| {
                Error::Invalid(format!(
                    "{} cannot be admitted: it has no {what}. Pass {flag}.",
                    row.label()
                ))
            };
            let scope = input
                .scope
                .clone()
                .or_else(|| row.scope.clone())
                .ok_or_else(|| missing("scope", "--scope"))?;
            let done_condition = input
                .done_when
                .clone()
                .or_else(|| row.done_condition.clone())
                .ok_or_else(|| missing("done condition", "--done-when"))?;
            let for_agent = match &input.for_agent {
                Some(raw) => {
                    Some(named_agent(tx, tx.node_id, raw, authority.agent_id(), "--for")?.id)
                }
                None => row.for_agent.clone(),
            };
            let priority = input.priority.unwrap_or(if row.kind.is_fast_lane() {
                QueuePriority::High
            } else {
                row.priority
            });
            let admitted_by = authority.agent_id();
            let self_admitted = admitted_by.is_some() && admitted_by == row.proposed_by.as_deref();
            record_tx(
                tx,
                &row,
                authority.actor(),
                "queue.admitted",
                json!({
                    "authority": authority.name(),
                    "admitted_by": admitted_by,
                    "self_admitted": self_admitted,
                    "via": authority.via(),
                    "scope": scope,
                    "done_condition": done_condition,
                    "priority": priority.as_str(),
                    "for_agent": for_agent,
                }),
                None,
                Some(json!({
                    "authority": authority.name(),
                    "admitted_by": admitted_by,
                    "self_admitted": self_admitted,
                    "via": authority.via(),
                    "priority": priority.as_str(),
                    "for_agent": for_agent,
                })),
            )?;
            tx.execute(
                "UPDATE queue_items SET state = 'ready', scope = ?2, done_condition = ?3,
                   priority = ?4, for_agent = ?5, admit_authority = ?6, admitted_by = ?7,
                   admitted_at = ?8, updated_at = ?8
                 WHERE item_id = ?1 AND state = 'proposed'",
                params![
                    row.item_id,
                    scope,
                    done_condition,
                    priority.rank(),
                    for_agent,
                    authority.name(),
                    admitted_by,
                    tx.now
                ],
            )?;
            if let Some(id) = admitted_by {
                touch(tx, id)?;
            }
            item_now(tx, tx.node_id, &row.item_id)
        })
    }

    /// Reject a proposed or ready item, with a reason. It is then done for good.
    pub fn queue_reject(
        &self,
        caller: &QueueCaller,
        item: &str,
        reason: &str,
    ) -> Result<QueueItem> {
        self.reject(caller, item, reason, None)
    }

    /// [`Store::queue_reject`] for a caller that was shown the item first.
    pub fn queue_reject_shown(
        &self,
        caller: &QueueCaller,
        item: &str,
        reason: &str,
        revision: &str,
    ) -> Result<QueueItem> {
        self.reject(caller, item, reason, Some(revision))
    }

    fn reject(
        &self,
        caller: &QueueCaller,
        item: &str,
        reason: &str,
        shown: Option<&str>,
    ) -> Result<QueueItem> {
        QueueItem::validate_reason(reason)?;
        self.write(|tx| {
            let authority = admitter(tx, tx.node_id, caller)?;
            let row = find_item(tx, tx.node_id, item)?;
            unchanged_since(tx, &row, shown)?;
            authority.may_decide(row.kind)?;
            if !matches!(row.state, QueueState::Proposed | QueueState::Ready) {
                return Err(wrong_state(
                    tx,
                    &row,
                    "only a proposed or ready item is rejected; release a claim first",
                )?);
            }
            let by = authority.agent_id();
            record_tx(
                tx,
                &row,
                authority.actor(),
                "queue.rejected",
                json!({
                    "authority": authority.name(),
                    "by": by,
                    "reason": reason,
                    "from_state": row.state.as_str(),
                }),
                Some(reason),
                Some(json!({ "authority": authority.name(), "from_state": row.state.as_str() })),
            )?;
            tx.execute(
                "UPDATE queue_items SET state = 'done', outcome = 'rejected', result_summary = ?2,
                   closed_by = ?3, closed_at = ?4, updated_at = ?4
                 WHERE item_id = ?1",
                params![row.item_id, reason, by, tx.now],
            )?;
            if let Some(id) = by {
                touch(tx, id)?;
            }
            item_now(tx, tx.node_id, &row.item_id)
        })
    }

    /// Edit a proposed or ready item: scope, done condition, priority, reservation, kind.
    /// Setting or clearing the reservation also clears the skip.
    pub fn queue_edit(
        &self,
        caller: &QueueCaller,
        item: &str,
        input: EditItem,
    ) -> Result<QueueItem> {
        self.edit(caller, item, input, None)
    }

    /// [`Store::queue_edit`] for a caller that was shown the item first.
    pub fn queue_edit_shown(
        &self,
        caller: &QueueCaller,
        item: &str,
        input: EditItem,
        revision: &str,
    ) -> Result<QueueItem> {
        self.edit(caller, item, input, Some(revision))
    }

    fn edit(
        &self,
        caller: &QueueCaller,
        item: &str,
        input: EditItem,
        shown: Option<&str>,
    ) -> Result<QueueItem> {
        input.validate()?;
        self.write(|tx| {
            let authority = admitter(tx, tx.node_id, caller)?;
            let row = find_item(tx, tx.node_id, item)?;
            unchanged_since(tx, &row, shown)?;
            authority.may_decide(row.kind)?;
            if !matches!(row.state, QueueState::Proposed | QueueState::Ready) {
                return Err(wrong_state(
                    tx,
                    &row,
                    "only a proposed or ready item is edited; release a claim first",
                )?);
            }
            let kind = input.kind.unwrap_or(row.kind);
            if kind != row.kind && row.state == QueueState::Ready {
                // A ready fast-lane item always carries the owner's own approval.
                match &authority {
                    Authority::Delegate(_) => {
                        return Err(Error::Forbidden(format!(
                            "only the owner changes the kind of a ready item; {} is ready",
                            row.label()
                        )))
                    }
                    Authority::Owner
                        if kind.is_fast_lane()
                            && row.admit_authority.as_deref() == Some("delegate") =>
                    {
                        return Err(Error::Forbidden(format!(
                            "{} was admitted by a delegate. Reject it and propose it again as {}.",
                            row.label(),
                            kind.flag()
                        )))
                    }
                    Authority::Owner => {}
                }
            }
            let scope = input.scope.clone().or_else(|| row.scope.clone());
            let done_condition = input
                .done_when
                .clone()
                .or_else(|| row.done_condition.clone());
            let priority = input.priority.unwrap_or(row.priority);
            let (for_agent, skip_agent) = match &input.for_agent {
                Some(raw) => (
                    Some(named_agent(tx, tx.node_id, raw, authority.agent_id(), "--for")?.id),
                    None,
                ),
                None if input.for_anyone => (None, None),
                None => (row.for_agent.clone(), row.skip_agent.clone()),
            };
            let mut changes = Map::new();
            let mut change = |field: &str, from: Value, to: Value| {
                if from != to {
                    changes.insert(field.into(), json!({ "from": from, "to": to }));
                }
            };
            change("scope", json!(row.scope), json!(scope));
            change(
                "done_condition",
                json!(row.done_condition),
                json!(done_condition),
            );
            change(
                "priority",
                json!(row.priority.as_str()),
                json!(priority.as_str()),
            );
            change("for_agent", json!(row.for_agent), json!(for_agent));
            change("skip_agent", json!(row.skip_agent), json!(skip_agent));
            change("kind", json!(row.kind.as_str()), json!(kind.as_str()));
            if !changes.is_empty() {
                let changes = Value::Object(changes);
                record_tx(
                    tx,
                    &row,
                    authority.actor(),
                    "queue.updated",
                    changes.clone(),
                    None,
                    Some(changes),
                )?;
                tx.execute(
                    "UPDATE queue_items SET scope = ?2, done_condition = ?3, priority = ?4,
                       for_agent = ?5, skip_agent = ?6, kind = ?7, updated_at = ?8
                     WHERE item_id = ?1",
                    params![
                        row.item_id,
                        scope,
                        done_condition,
                        priority.rank(),
                        for_agent,
                        skip_agent,
                        kind.as_str(),
                        tx.now
                    ],
                )?;
            }
            if let Some(id) = authority.agent_id() {
                touch(tx, id)?;
            }
            item_now(tx, tx.node_id, &row.item_id)
        })
    }

    /// Hand the first ready item the agent may take to the agent, or say why not. The only
    /// way into `claimed`. One write transaction: two callers never get the same item.
    pub fn queue_next(&self, agent: &str) -> Result<QueueNext> {
        self.queue_next_with_ttl(agent, None)
    }

    pub fn queue_next_with_ttl(&self, agent: &str, ttl_seconds: Option<i64>) -> Result<QueueNext> {
        let ttl = crate::checkout_ttl(ttl_seconds)?;
        self.write(|tx| {
            let me = crate::identity::resolve_acting_agent(tx, tx.node_id, agent)?;
            crate::actor_leases::prepare_business_subject(tx, &me.id, crate::SubjectKind::Queue, "")?;
            touch(tx, &me.id)?;
            // After the caller's own sign of life, and never against the caller's claims.
            queue_sweep_tx(tx, Some(&me.id))?;
            let standing = standing(tx, &me.id)?;
            let (limit, held) = (standing.limit, standing.labels());
            let refuse = |reason: &str, message: String| {
                Ok(QueueNext {
                    claimed: None,
                    refused: Some(QueueRefusal {
                        reason: reason.to_string(),
                        limit,
                        held: held.clone(),
                        message,
                    }),
                })
            };
            let who = &me.address;
            if limit == 0 {
                return refuse(
                    "no_seat",
                    format!(
                        "{who} has no seat in the queue: its limit is 0. The owner gives it one with another queue limit set {who} 1 --owner."
                    ),
                );
            }
            if !standing.has_room() {
                let items = held.join(", ");
                return refuse(
                    "at_limit",
                    format!(
                        "{who} holds {} of its limit of {limit}: {items}. Finish one with another queue done <item> --summary TEXT and a pointer (--pr, --commit or --file), or give one back with another queue release <item>. If another session holds the item, the two sessions share the name {who}: give each its own ANOTHER_AGENT.",
                        held.len()
                    ),
                );
            }
            let Some(row) = first_ready(tx, tx.node_id, &me.id, tx.now)? else {
                return Ok(QueueNext {
                    claimed: None,
                    refused: None,
                });
            };
            let session = fresh_session(tx, &me.id, tx.now)?;
            crate::actor_leases::projection_checkout(tx, &me.id, crate::SubjectKind::Queue, &row.item_id, ttl, &format!("queue:{}", row.item_id))?;
            let changed = tx.execute(
                "UPDATE queue_items SET state = 'claimed', claimer_id = ?2, claimed_at = ?3,
                   claim_session = ?4, skip_agent = NULL, updated_at = ?3
                 WHERE item_id = ?1 AND state = 'ready'",
                params![row.item_id, me.id, tx.now, session],
            )?;
            if changed != 1 {
                return Err(Error::Conflict(format!(
                    "claiming {} changed {changed} rows, not one; nothing was claimed. Run the command again",
                    row.label()
                )));
            }
            record_tx(
                tx,
                &row,
                &me.address,
                "queue.claimed",
                json!({
                    "claimer_id": me.id,
                    "session": session,
                    "held": held.len(),
                    "limit": limit,
                }),
                None,
                Some(json!({ "held": held.len(), "limit": limit, "session": session })),
            )?;
            Ok(QueueNext {
                claimed: Some(item_now(tx, tx.node_id, &row.item_id)?),
                refused: None,
            })
        })
    }

    /// What [`Store::queue_next`] would do for the agent now, without doing it. It only
    /// reads: no sign of life, no sweep, no event. So where a claim of another agent is
    /// past the grace period, `next` would release it first and may hand over an item
    /// this read does not see; the read never offers an item `next` would not give.
    pub fn queue_offer(&self, agent: &str) -> Result<QueueOffer> {
        let now = self.now();
        let node_id = self.node_id();
        self.read(|conn| {
            let me = resolve_agent(conn, node_id, agent)?;
            let standing = standing(conn, &me.id)?;
            let ready = if standing.has_room() {
                match first_ready(conn, node_id, &me.id, now)? {
                    Some(row) => {
                        let back = given_up_at(conn, &row.item_id)?.unwrap_or(0);
                        Some(format!("{}:{back}", row.item_id))
                    }
                    None => None,
                }
            } else {
                None
            };
            Ok(QueueOffer {
                agent: me.address,
                seated: standing.own_limit.is_some_and(|n| n > 0),
                limit: standing.limit,
                held: standing
                    .held
                    .into_iter()
                    .map(|(seq, session)| HeldClaim {
                        label: label(seq),
                        session,
                    })
                    .collect(),
                ready,
            })
        })
    }

    /// Add a note to an item that is not done. Any local agent may.
    pub fn queue_note(&self, agent: &str, item: &str, body: &str) -> Result<QueueItem> {
        check_text("the note", body, MAX_NOTE_CHARS, true)?;
        self.write(|tx| {
            let me = crate::identity::resolve_acting_agent(tx, tx.node_id, agent)?;
            let row = find_item(tx, tx.node_id, item)?;
            if row.state == QueueState::Done {
                return Err(wrong_state(tx, &row, "a finished item takes no notes")?);
            }
            record_tx(
                tx,
                &row,
                &me.address,
                "queue.noted",
                json!({ "agent_id": me.id, "note": body }),
                Some(body),
                None,
            )?;
            tx.execute(
                "UPDATE queue_items SET updated_at = ?2 WHERE item_id = ?1",
                params![row.item_id, tx.now],
            )?;
            touch(tx, &me.id)?;
            item_now(tx, tx.node_id, &row.item_id)
        })
    }

    /// Return a claimed item to `ready`. The claimer gives it back, or hands it to another
    /// agent with `to`. An admitter who is not the claimer releases it with a `reason`, and
    /// the former claimer is told by mail.
    pub fn queue_release(
        &self,
        caller: &QueueCaller,
        item: &str,
        input: ReleaseItem,
    ) -> Result<QueueItem> {
        self.release(caller, item, input, None)
    }

    /// [`Store::queue_release`] for a caller that was shown the item first.
    pub fn queue_release_shown(
        &self,
        caller: &QueueCaller,
        item: &str,
        input: ReleaseItem,
        revision: &str,
    ) -> Result<QueueItem> {
        self.release(caller, item, input, Some(revision))
    }

    fn release(
        &self,
        caller: &QueueCaller,
        item: &str,
        input: ReleaseItem,
        shown: Option<&str>,
    ) -> Result<QueueItem> {
        input.validate()?;
        self.write(|tx| {
            let row = find_item(tx, tx.node_id, item)?;
            unchanged_since(tx, &row, shown)?;
            let claimer = match caller {
                QueueCaller::Agent { agent, .. } => {
                    let me = crate::identity::resolve_acting_agent(tx, tx.node_id, agent)?;
                    if matches!(tx.caller, crate::store::Caller::Actor(_)) {
                        return Err(Error::Invalid("NO_CALL_CONTEXT: queue handoff requires exact claim generation carriage; use exact lease release".into()));
                    }
                    (row.claimer_id.as_deref() == Some(me.id.as_str())).then_some(me)
                }
                QueueCaller::Owner => None,
            };
            let Some(me) = claimer else {
                return admin_release_tx(tx, caller, row, &input);
            };
            let Some(claimed_at) = row.claimed_at.filter(|_| row.state == QueueState::Claimed)
            else {
                return Err(wrong_state(tx, &row, "only a claimed item is released")?);
            };
            if input.reason.is_some() {
                return Err(Error::Invalid(
                    "--reason is for an admitter releasing another agent's claim; add your own words with --note".into(),
                ));
            }
            match &input.to {
                None => {
                    let cleared = row.for_agent.as_deref().filter(|f| *f == me.id);
                    let for_agent = match cleared {
                        Some(_) => None,
                        None => row.for_agent.as_deref(),
                    };
                    record_tx(
                        tx,
                        &row,
                        &me.address,
                        "queue.released",
                        json!({
                            "reason": "agent",
                            "previous_claimer_id": me.id,
                            "claimed_at": claimed_at,
                            "note": input.note,
                            "evidence": null,
                            "for_agent_cleared": cleared,
                            "notice_message_id": null,
                        }),
                        input.note.as_deref(),
                        Some(json!({ "reason": "agent", "for_agent_cleared": cleared })),
                    )?;
                    unclaim_tx(tx, &row, for_agent, Some(&me.id))?;
                }
                Some(raw) => {
                    let to = named_agent(tx, tx.node_id, raw, Some(&me.id), "--to")?;
                    unclaim_tx(tx, &row, Some(&to.id), Some(&me.id))?;
                    let notice = notify_tx(
                        tx,
                        &row,
                        &me.id,
                        &to.id,
                        "handoff",
                        format!("Handoff: {}", row.label()),
                        format!(
                            "{} handed {} to you. Run another queue next.",
                            me.address,
                            row.label()
                        ),
                    )?;
                    record_tx(
                        tx,
                        &row,
                        &me.address,
                        "queue.handoff",
                        json!({
                            "from": me.id,
                            "to": to.id,
                            "note": input.note,
                            "claimed_at": claimed_at,
                            "notice_message_id": notice,
                        }),
                        input.note.as_deref(),
                        Some(json!({ "to": to.id })),
                    )?;
                }
            }
            touch(tx, &me.id)?;
            item_now(tx, tx.node_id, &row.item_id)
        })
    }

    /// Finish a claimed item with its result: a summary and one pointer, stored on the item
    /// and in the event. The claimer's alone.
    pub fn queue_done(&self, agent: &str, item: &str, input: DoneItem) -> Result<QueueItem> {
        self.queue_done_with_lease(agent, item, input, None)
    }

    pub fn queue_done_with_lease(
        &self,
        agent: &str,
        item: &str,
        input: DoneItem,
        expected: Option<&crate::LeaseRef>,
    ) -> Result<QueueItem> {
        check_text("the summary", &input.summary, MAX_SUMMARY_CHARS, true)?;
        check_pointer(input.pointer_kind, &input.pointer)?;
        self.write(|tx| {
            let me = crate::identity::resolve_acting_agent(tx, tx.node_id, agent)?;
            let row = find_item(tx, tx.node_id, item)?;
            crate::actor_leases::validate_projection(
                tx,
                &me.id,
                &format!("queue:{}", row.item_id),
                expected,
            )?;
            let (Some(claimer), Some(claimed_at)) = (
                row.claimer_id
                    .as_deref()
                    .filter(|_| row.state == QueueState::Claimed),
                row.claimed_at,
            ) else {
                return Err(wrong_state(
                    tx,
                    &row,
                    "only a claimed item is finished; take it with another queue next",
                )?);
            };
            if claimer != me.id {
                return Err(Error::Forbidden(format!(
                    "{} is {}, not by {}; only the claimer finishes it",
                    row.label(),
                    state_words(tx, &row)?,
                    me.address
                )));
            }
            crate::actor_leases::projection_release(tx, &format!("queue:{}", row.item_id), "done")?;
            record_tx(
                tx,
                &row,
                &me.address,
                "queue.completed",
                json!({
                    "claimer_id": me.id,
                    "claimed_at": claimed_at,
                    "summary": input.summary,
                    "pointer_kind": input.pointer_kind.as_str(),
                    "pointer": input.pointer,
                }),
                Some(input.summary.as_str()),
                Some(json!({
                    "pointer_kind": input.pointer_kind.as_str(),
                    "pointer": input.pointer,
                })),
            )?;
            tx.execute(
                "UPDATE queue_items SET state = 'done', outcome = 'completed', result_summary = ?2,
                   result_kind = ?3, result_ref = ?4, closed_by = ?5, closed_at = ?6,
                   claimer_id = NULL, claimed_at = NULL, claim_session = NULL, updated_at = ?6
                 WHERE item_id = ?1",
                params![
                    row.item_id,
                    input.summary,
                    input.pointer_kind.as_str(),
                    input.pointer,
                    me.id,
                    tx.now
                ],
            )?;
            touch(tx, &me.id)?;
            item_now(tx, tx.node_id, &row.item_id)
        })
    }

    /// Set how many claimed, unfinished items one agent may hold (0 to 20; 0 is no seat).
    /// The owner's alone. Lowering a limit releases nothing.
    pub fn queue_set_limit(
        &self,
        caller: &QueueCaller,
        agent: &str,
        limit: i64,
    ) -> Result<QueueLimits> {
        owner_only(caller, "setting a limit")?;
        check_limit(limit)?;
        self.write(|tx| {
            let who = resolve_agent(tx, tx.node_id, agent)?;
            let from = limit_of(tx, &who.id)?;
            let event_id = tx.emit(
                "owner",
                "queue.limit_set",
                json!({ "agent_id": who.id, "from": from, "to": limit }),
            )?;
            tx.execute(
                "INSERT INTO queue_limits (agent_id, max_claimed, set_at, event_id)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(agent_id) DO UPDATE SET max_claimed = ?2, set_at = ?3, event_id = ?4",
                params![who.id, limit, tx.now, event_id],
            )?;
            limits(tx, tx.node_id)
        })
    }

    /// Set the limit of every agent the owner has set none for. The owner's alone.
    pub fn queue_set_default_limit(&self, caller: &QueueCaller, limit: i64) -> Result<QueueLimits> {
        owner_only(caller, "setting the default limit")?;
        check_limit(limit)?;
        self.write(|tx| {
            let from = default_limit(tx)?;
            let event_id = tx.emit(
                "owner",
                "queue.limit_set",
                json!({ "default": true, "from": from, "to": limit }),
            )?;
            set_setting_tx(tx, "default_limit", limit, &event_id)?;
            limits(tx, tx.node_id)
        })
    }

    /// The default limit and each limit the owner has set. Read only.
    pub fn queue_limits(&self) -> Result<QueueLimits> {
        self.read(|conn| limits(conn, self.node_id()))
    }

    /// Name a delegate and return its key, which is shown once. Adding an agent again gives
    /// it a new key. The owner's alone.
    pub fn queue_delegate_add(
        &self,
        caller: &QueueCaller,
        agent: &str,
    ) -> Result<NewQueueDelegate> {
        owner_only(caller, "naming a delegate")?;
        let key = format!("adk_{}", crate::crypto::random_hex(32)?);
        self.write(|tx| {
            let who = resolve_agent(tx, tx.node_id, agent)?;
            let event_id = tx.emit(
                "owner",
                "queue.delegate_added",
                json!({ "agent_id": who.id }),
            )?;
            tx.execute(
                "INSERT INTO queue_delegates (agent_id, key_hash, added_at, removed_at, event_id)
                 VALUES (?1, ?2, ?3, NULL, ?4)
                 ON CONFLICT(agent_id) DO UPDATE SET key_hash = ?2, added_at = ?3,
                   removed_at = NULL, event_id = ?4",
                params![
                    who.id,
                    crate::crypto::sha256_hex(key.as_bytes()),
                    tx.now,
                    event_id
                ],
            )?;
            Ok(NewQueueDelegate {
                agent: who.address,
                key: key.clone(),
            })
        })
    }

    /// End a delegation; its key stops working. The owner's alone.
    pub fn queue_delegate_remove(
        &self,
        caller: &QueueCaller,
        agent: &str,
    ) -> Result<Vec<QueueDelegate>> {
        owner_only(caller, "removing a delegate")?;
        self.write(|tx| {
            let who = resolve_agent(tx, tx.node_id, agent)?;
            let active: Option<i64> = tx
                .query_row(
                    "SELECT 1 FROM queue_delegates WHERE agent_id = ?1 AND removed_at IS NULL",
                    [&who.id],
                    |r| r.get(0),
                )
                .optional()?;
            if active.is_none() {
                return Err(Error::NotFound(format!(
                    "unknown delegate {:?}: list them with another queue delegate list",
                    who.address
                )));
            }
            let event_id = tx.emit(
                "owner",
                "queue.delegate_removed",
                json!({ "agent_id": who.id }),
            )?;
            tx.execute(
                "UPDATE queue_delegates SET removed_at = ?2, event_id = ?3 WHERE agent_id = ?1",
                params![who.id, tx.now, event_id],
            )?;
            delegates(tx)
        })
    }

    /// The agents that hold a delegate key. Read only.
    pub fn queue_delegates(&self) -> Result<Vec<QueueDelegate>> {
        self.read(delegates)
    }

    /// The grace period. Read only.
    pub fn queue_grace(&self) -> Result<QueueGrace> {
        self.read(|conn| {
            Ok(QueueGrace {
                grace_seconds: grace_seconds(conn)?,
            })
        })
    }

    /// Change the grace period (2 hours to 30 days). The owner's alone.
    pub fn queue_set_grace(&self, caller: &QueueCaller, seconds: i64) -> Result<QueueGrace> {
        owner_only(caller, "changing the grace period")?;
        QueueGrace::validate(seconds)?;
        self.write(|tx| {
            let from = grace_seconds(tx)?;
            let event_id = tx.emit(
                "owner",
                "queue.setting_changed",
                json!({ "key": "grace_seconds", "from": from, "to": seconds }),
            )?;
            set_setting_tx(tx, "grace_seconds", seconds, &event_id)?;
            Ok(QueueGrace {
                grace_seconds: seconds,
            })
        })
    }

    /// One item as a person is about to be shown it, with its revision. A write that
    /// carries the revision (`queue_admit_shown` and the others) is refused if the item has
    /// changed since. The revision is read first, so a change that lands during this read
    /// makes the later write fail, never pass. Read only.
    pub fn queue_item_shown(&self, item: &str) -> Result<(QueueItem, String)> {
        self.read(|conn| {
            let item_id = find_item(conn, self.node_id(), item)?.item_id;
            let revision = revision_of(conn, &item_id)?;
            Ok((item_now(conn, self.node_id(), &item_id)?, revision))
        })
    }

    /// One item with its history, by `Q` number or id. Read only.
    pub fn queue_item(&self, item: &str) -> Result<QueueItem> {
        self.read(|conn| item_now(conn, self.node_id(), item))
    }

    /// List items. Read only: it writes nothing and runs no sweep.
    pub fn queue_list(&self, filter: QueueFilter) -> Result<Vec<QueueItem>> {
        if let Some(scope) = &filter.scope {
            check_scope(scope)?;
        }
        self.read(|conn| {
            let mine = match &filter.mine {
                Some(who) => Some(resolve_agent(conn, self.node_id(), who)?.id),
                None => None,
            };
            let open_only = filter.state.is_none() && mine.is_none() && !filter.self_admitted;
            let limit = if filter.limit == 0 { 100 } else { filter.limit };
            let mut stmt = conn.prepare(&format!(
                "SELECT {ITEM_COLUMNS} FROM queue_items
                 WHERE node_id = ?1
                   AND (?2 IS NULL OR state = ?2)
                   AND (?3 = 0 OR state IN ('ready', 'claimed'))
                   AND (?4 IS NULL OR scope = ?4)
                   AND (?5 IS NULL OR proposed_by = ?5 OR claimer_id = ?5)
                   AND (?6 = 0 OR (admit_authority = 'delegate' AND admitted_by = proposed_by))
                 ORDER BY CASE state WHEN 'claimed' THEN 0 WHEN 'ready' THEN 1
                            WHEN 'proposed' THEN 2 ELSE 3 END,
                          priority, coalesce(admitted_at, proposed_at), seq
                 LIMIT ?7"
            ))?;
            let rows = stmt
                .query_map(
                    params![
                        self.node_id(),
                        filter.state.map(QueueState::as_str),
                        open_only,
                        filter.scope,
                        mine,
                        filter.self_admitted,
                        limit
                    ],
                    row_from,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().map(|r| to_item(conn, r, false)).collect()
        })
    }

    /// Every claimed item of this node with its claimer and the session the claim is bound
    /// to, oldest claim first. No limit: [`Store::queue_list`] stops at 100 rows. Read only.
    pub fn queue_claims(&self) -> Result<Vec<QueueClaimed>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {ITEM_COLUMNS} FROM queue_items
                 WHERE node_id = ?1 AND state = 'claimed' AND claimer_id IS NOT NULL
                 ORDER BY claimed_at, seq"
            ))?;
            let rows = stmt
                .query_map([self.node_id()], row_from)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut claims = Vec::new();
            for row in rows {
                let Some(claimer) = &row.claimer_id else {
                    continue;
                };
                claims.push(QueueClaimed {
                    item: row.label(),
                    agent: address_of(conn, claimer)?,
                    session: match session_seen(conn, &row)? {
                        Some(_) => row.claim_session.clone(),
                        None => None,
                    },
                });
            }
            Ok(claims)
        })
    }

    /// What needs the owner: decisions waiting, quiet claims, ready items nobody can take,
    /// failed notices and self-admitted items. Read only; listing releases nothing.
    pub fn queue_needs_owner(&self) -> Result<QueueNeedsOwner> {
        let now = self.now();
        self.read(|conn| {
            let node = self.node_id();
            let items = |filter: &str, order: &str| -> Result<Vec<QueueItem>> {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {ITEM_COLUMNS} FROM queue_items WHERE node_id = ?1 AND {filter}
                     ORDER BY {order}"
                ))?;
                let rows = stmt
                    .query_map([node], row_from)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows.into_iter().map(|r| to_item(conn, r, false)).collect()
            };
            let decide_now = items("state = 'proposed' AND kind != 'work'", "proposed_at, seq")?;
            let awaiting = items("state = 'proposed' AND kind = 'work'", "proposed_at, seq")?;

            let grace_ms = grace_seconds(conn)?.saturating_mul(1000);
            let mut stmt = conn.prepare(&format!(
                "SELECT {ITEM_COLUMNS} FROM queue_items WHERE node_id = ?1 AND state = 'claimed'
                 ORDER BY claimed_at, seq"
            ))?;
            let claimed = stmt
                .query_map([node], row_from)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut quiet_claims = Vec::new();
            for row in claimed {
                let (Some(claimer), Some(claimed_at)) = (&row.claimer_id, row.claimed_at) else {
                    continue;
                };
                let last_sign = claimed_at.max(last_seen(conn, claimer)?.unwrap_or(0));
                // A bound claim whose session row is gone is treated as unbound.
                let session_seen = session_seen(conn, &row)?;
                let quiet = |since: i64| now.saturating_sub(since) >= QUIET_MS;
                // Listed, by the first that holds: a quiet name; a claim whose own session
                // is quiet though the name is active; a claim no session's end will
                // release, once it is a grace period old. A younger unbound claim under an
                // active name is not listed: nothing stored says which session made it.
                let reason = if quiet(last_sign) {
                    "name_quiet"
                } else if session_seen.is_some_and(quiet) {
                    "session_quiet"
                } else if session_seen.is_none() && now.saturating_sub(claimed_at) >= grace_ms {
                    "old_unbound"
                } else {
                    continue;
                };
                quiet_claims.push(QuietClaim {
                    quiet_for_seconds: now.saturating_sub(last_sign) / 1000,
                    grace_ends_at: fmt_ts(last_sign.saturating_add(grace_ms)),
                    unbound: session_seen.is_none(),
                    session_last_seen_at: session_seen.map(fmt_ts),
                    reason: reason.to_string(),
                    session_quiet_for_seconds: session_seen
                        .map(|seen| now.saturating_sub(seen) / 1000),
                    claimed_for_seconds: now.saturating_sub(claimed_at) / 1000,
                    item: to_item(conn, row, false)?,
                });
            }

            // Agents with a seat, and when each was last seen.
            let mut stmt =
                conn.prepare("SELECT agent_id, last_seen_at FROM agents WHERE node_id = ?1")?;
            let agents = stmt
                .query_map([node], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut seated = Vec::new();
            for (id, seen) in agents {
                if limit_of(conn, &id)? > 0 {
                    seated.push((id, seen));
                }
            }
            let mut stmt = conn.prepare(&format!(
                "SELECT {ITEM_COLUMNS} FROM queue_items WHERE node_id = ?1 AND state = 'ready'
                 ORDER BY seq"
            ))?;
            let ready = stmt
                .query_map([node], row_from)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut nobody_can_take = Vec::new();
            for row in ready {
                let skip = match skip_holds(conn, &row, now)? {
                    true => row.skip_agent.as_deref(),
                    false => None,
                };
                let stuck = match row.for_agent.as_deref() {
                    Some(reserved) => !seated.iter().any(|(id, seen)| {
                        id == reserved
                            && skip != Some(reserved)
                            && seen.is_some_and(|t| now.saturating_sub(t) < QUIET_MS)
                    }),
                    None => !seated.iter().any(|(id, _)| skip != Some(id.as_str())),
                };
                if stuck {
                    nobody_can_take.push(to_item(conn, row, false)?);
                }
            }

            let mut stmt = conn.prepare(
                "SELECT ts, payload FROM events
                 WHERE node_id = ?1 AND kind = 'queue.notify_failed' AND ts >= ?2 ORDER BY seq",
            )?;
            let failed = stmt
                .query_map(params![node, now - FAILED_NOTICE_WINDOW_MS], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut failed_notices = Vec::new();
            for (ts, payload) in failed {
                let p: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
                let item = match p.get("seq").and_then(Value::as_i64) {
                    Some(seq) => label(seq),
                    None => p
                        .get("item_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                };
                let to = p.get("to").and_then(Value::as_str).unwrap_or_default();
                failed_notices.push(FailedNotice {
                    item,
                    to: address_of(conn, to)?,
                    at: fmt_ts(ts),
                });
            }

            let self_admitted = "admit_authority = 'delegate' AND admitted_by = proposed_by";
            let recent = items(
                &format!(
                    "{self_admitted} AND admitted_at >= {}",
                    now - SELF_ADMITTED_RECENT_MS
                ),
                "admitted_at DESC, seq DESC",
            )?;
            let total: u32 = conn.query_row(
                &format!("SELECT count(*) FROM queue_items WHERE node_id = ?1 AND {self_admitted}"),
                [node],
                |r| r.get(0),
            )?;
            Ok(QueueNeedsOwner {
                decide_now,
                awaiting,
                quiet_claims,
                nobody_can_take,
                failed_notices,
                self_admitted: SelfAdmitted { recent, total },
            })
        })
    }

    /// Check this node's `queue.*` events against themselves and against the tables:
    /// duplicate claims, lost items, claims of items nobody admitted, claims over a limit.
    /// Every event is read; `since` (ms) only limits which findings are reported.
    /// Read only.
    pub fn queue_audit(&self, since: Option<i64>) -> Result<QueueAudit> {
        self.read(|conn| audit(conn, self.node_id(), since))
    }
}

/// Release another agent's claim as an admitter, and mail the former claimer.
fn admin_release_tx(
    tx: &Tx<'_>,
    caller: &QueueCaller,
    row: ItemRow,
    input: &ReleaseItem,
) -> Result<QueueItem> {
    // An agent with no key is told who holds the item, not that it lacks a key.
    if let QueueCaller::Agent {
        agent,
        admit_key: None,
    } = caller
    {
        if row.state != QueueState::Claimed {
            return Err(wrong_state(tx, &row, "only a claimed item is released")?);
        }
        let me = crate::identity::resolve_acting_agent(tx, tx.node_id, agent)?;
        return Err(Error::Forbidden(format!(
            "{} is {}, not by {}; only the claimer or an admitter releases it",
            row.label(),
            state_words(tx, &row)?,
            me.address
        )));
    }
    let authority = admitter(tx, tx.node_id, caller)?;
    authority.may_decide(row.kind)?;
    let (Some(former), Some(claimed_at)) = (
        row.claimer_id
            .clone()
            .filter(|_| row.state == QueueState::Claimed),
        row.claimed_at,
    ) else {
        return Err(wrong_state(tx, &row, "only a claimed item is released")?);
    };
    if input.to.is_some() {
        return Err(Error::Invalid(
            "--to is the claimer's; an admitter releases another agent's claim with --reason, and reserves it with another queue edit --for".into(),
        ));
    }
    let Some(reason) = input.reason.as_deref() else {
        return Err(Error::Invalid(format!(
            "releasing {}, which is {}, needs --reason",
            row.label(),
            state_words(tx, &row)?
        )));
    };
    let cleared = row.for_agent.as_deref().filter(|f| *f == former);
    let for_agent = match cleared {
        Some(_) => None,
        None => row.for_agent.as_deref(),
    };
    unclaim_tx(tx, &row, for_agent, Some(&former))?;
    // The system has no agent: the notice comes from the delegate that released the claim,
    // or, when the owner did, from the former claimer itself.
    let from = authority.agent_id().unwrap_or(&former);
    let notice = notify_tx(
        tx,
        &row,
        from,
        &former,
        "owner",
        format!("Claim released: {}", row.label()),
        format!(
            "Your claim on {} was released at {}: an admitter released it. The claim and its notes are kept in the item's history. It will not be offered to you again for the next hour unless an admitter reserves it for you. {AUTOMATIC}",
            row.label(),
            fmt_ts(tx.now)
        ),
    )?;
    record_tx(
        tx,
        &row,
        authority.actor(),
        "queue.released",
        json!({
            "reason": "owner",
            "authority": authority.name(),
            "previous_claimer_id": former,
            "claimed_at": claimed_at,
            "note": reason,
            "evidence": null,
            "for_agent_cleared": cleared,
            "notice_message_id": notice,
        }),
        Some(reason),
        Some(json!({
            "reason": "owner",
            "authority": authority.name(),
            "for_agent_cleared": cleared,
        })),
    )?;
    if let Some(id) = authority.agent_id() {
        touch(tx, id)?;
    }
    item_now(tx, tx.node_id, &row.item_id)
}

fn check_limit(limit: i64) -> Result<()> {
    if (0..=MAX_QUEUE_LIMIT).contains(&limit) {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "a limit is a number from 0 to {MAX_QUEUE_LIMIT}; 0 means no seat"
        )))
    }
}

fn last_seen(conn: &Connection, agent_id: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT last_seen_at FROM agents WHERE agent_id = ?1",
            [agent_id],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten())
}

fn limits(conn: &Connection, node_id: &str) -> Result<QueueLimits> {
    let mut stmt = conn.prepare(
        "SELECT coalesce(a.address, a.name), l.max_claimed, l.set_at
         FROM queue_limits l JOIN agents a ON a.agent_id = l.agent_id
         WHERE a.node_id = ?1 ORDER BY 1",
    )?;
    let agents = stmt
        .query_map([node_id], |r| {
            Ok(QueueLimit {
                agent: r.get(0)?,
                limit: r.get(1)?,
                set_at: fmt_ts(r.get(2)?),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(QueueLimits {
        default_limit: default_limit(conn)?,
        agents,
    })
}

fn delegates(conn: &Connection) -> Result<Vec<QueueDelegate>> {
    let mut stmt = conn.prepare(
        "SELECT coalesce(a.address, a.name), d.added_at
         FROM queue_delegates d JOIN agents a ON a.agent_id = d.agent_id
         WHERE d.removed_at IS NULL ORDER BY 1",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(QueueDelegate {
            agent: r.get(0)?,
            added_at: fmt_ts(r.get(1)?),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// What the event log says about one item.
#[derive(Default)]
struct Tracked {
    seq: Option<i64>,
    proposed: bool,
    admitted: bool,
    state: Option<QueueState>,
    claimer: Option<String>,
    last_ts: i64,
    /// The events that each have a history row.
    events: BTreeSet<String>,
}

/// The event kinds that are a transition or a note, and so have a history row.
const HISTORY_KINDS: [&str; 9] = [
    "queue.proposed",
    "queue.admitted",
    "queue.rejected",
    "queue.updated",
    "queue.claimed",
    "queue.noted",
    "queue.released",
    "queue.handoff",
    "queue.completed",
];

fn audit(conn: &Connection, node_id: &str, since: Option<i64>) -> Result<QueueAudit> {
    let mut stmt = conn.prepare(
        "SELECT event_id, ts, kind, payload FROM events
         WHERE node_id = ?1 AND kind LIKE 'queue.%' ORDER BY seq",
    )?;
    let events = stmt
        .query_map([node_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut tracked: BTreeMap<String, Tracked> = BTreeMap::new();
    let mut found: Vec<(i64, AuditFinding)> = Vec::new();
    let mut claims = 0u32;
    let mut completions = 0u32;
    let mut releases: BTreeMap<String, u32> = BTreeMap::new();
    let mut claimers: BTreeSet<String> = BTreeSet::new();
    let name = |t: &Tracked, item_id: &str| t.seq.map_or_else(|| item_id.to_string(), label);
    for (event_id, ts, kind, payload) in events {
        if !HISTORY_KINDS.contains(&kind.as_str()) {
            continue;
        }
        let p: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
        let Some(item_id) = p.get("item_id").and_then(Value::as_str) else {
            continue;
        };
        let t = tracked.entry(item_id.to_string()).or_default();
        t.seq = t.seq.or(p.get("seq").and_then(Value::as_i64));
        t.last_ts = ts;
        t.events.insert(event_id);
        let item_name = name(t, item_id);
        let mut finding = |kind: &str, detail: String| {
            found.push((
                ts,
                AuditFinding {
                    kind: kind.to_string(),
                    item: item_name.clone(),
                    at: fmt_ts(ts),
                    detail,
                },
            ));
        };
        match kind.as_str() {
            "queue.proposed" => {
                t.proposed = true;
                t.state = Some(QueueState::Proposed);
            }
            "queue.admitted" => {
                t.admitted = true;
                t.state = Some(QueueState::Ready);
            }
            "queue.rejected" => t.state = Some(QueueState::Done),
            "queue.claimed" => {
                let claimer = p
                    .get("claimer_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if t.state == Some(QueueState::Claimed) {
                    finding(
                        "duplicate",
                        "claimed twice with no release or handoff between".into(),
                    );
                }
                if !t.admitted {
                    finding("admission", "claimed with no earlier queue.admitted".into());
                }
                let held = p.get("held").and_then(Value::as_i64).unwrap_or(0);
                let limit = p.get("limit").and_then(Value::as_i64).unwrap_or(i64::MAX);
                if held >= limit {
                    finding(
                        "limit",
                        format!("claimed while the agent held {held} of its limit of {limit}"),
                    );
                }
                claims += 1;
                claimers.insert(claimer.clone());
                t.state = Some(QueueState::Claimed);
                t.claimer = Some(claimer);
            }
            "queue.released" | "queue.handoff" => {
                let reason = if kind == "queue.handoff" {
                    "handoff"
                } else {
                    p.get("reason").and_then(Value::as_str).unwrap_or("unknown")
                };
                *releases.entry(reason.to_string()).or_default() += 1;
                t.state = Some(QueueState::Ready);
                t.claimer = None;
            }
            "queue.completed" => {
                completions += 1;
                t.state = Some(QueueState::Done);
                t.claimer = None;
            }
            _ => {}
        }
    }

    for (item_id, t) in &tracked {
        let mut loss = |detail: String| {
            found.push((
                t.last_ts,
                AuditFinding {
                    kind: "loss".into(),
                    item: name(t, item_id),
                    at: fmt_ts(t.last_ts),
                    detail,
                },
            ));
        };
        let row: Option<(String, Option<String>)> = conn
            .query_row(
                "SELECT state, claimer_id FROM queue_items WHERE item_id = ?1 AND node_id = ?2",
                params![item_id, node_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((state, claimer)) = row else {
            if t.proposed {
                loss("the log has its queue.proposed event and the table has no row".into());
            }
            continue;
        };
        let expected = t.state.map_or("none", QueueState::as_str);
        if state != expected || claimer != t.claimer {
            loss(format!(
                "the row is {state} (claimer {}) and its last state-changing event says {expected} (claimer {})",
                claimer.as_deref().unwrap_or("none"),
                t.claimer.as_deref().unwrap_or("none")
            ));
        }
        let mut stmt = conn.prepare("SELECT event_id FROM queue_history WHERE item_id = ?1")?;
        let history = stmt
            .query_map([item_id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<BTreeSet<_>>>()?;
        if history != t.events {
            loss(format!(
                "its history has {} row(s) and the log has {} event(s) for it",
                history.len(),
                t.events.len()
            ));
        }
    }
    // A row the log never proposed.
    let mut stmt =
        conn.prepare("SELECT item_id, seq, updated_at FROM queue_items WHERE node_id = ?1")?;
    let rows = stmt
        .query_map([node_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (item_id, seq, updated_at) in rows {
        if !tracked.contains_key(&item_id) {
            found.push((
                updated_at,
                AuditFinding {
                    kind: "loss".into(),
                    item: label(seq),
                    at: fmt_ts(updated_at),
                    detail: "the table has a row and the log has no event for it".into(),
                },
            ));
        }
    }

    found.sort_by_key(|(ts, _)| *ts);
    let findings: Vec<AuditFinding> = found
        .into_iter()
        .filter(|(ts, _)| since.is_none_or(|s| *ts >= s))
        .map(|(_, f)| f)
        .collect();
    Ok(QueueAudit {
        ok: findings.is_empty(),
        findings,
        items: tracked.values().filter(|t| t.proposed).count() as u32,
        claims,
        releases,
        completions,
        claimers: claimers.len() as u32,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::Program;

    /// The queue sweep never releases a claim of the agent whose call runs it, even when
    /// that claim is overdue. Every caller touches the agent first, so only a sweep run
    /// here without that touch shows the rule on its own.
    #[test]
    fn the_queue_sweep_skips_the_calling_agents_claims() {
        let (clock, now) = crate::store::Clock::manual(1_800_000_000_000);
        let store = Store::open_in_memory(clock).unwrap().unguarded_repair();
        for name in ["ann", "bob"] {
            store
                .register_agent(crate::RegisterAgent {
                    name: name.into(),
                    program: Program::Claude,
                    project: None,
                    model: None,
                    description: None,
                })
                .unwrap();
        }
        for (title, who) in [("first", "ann"), ("second", "bob")] {
            let item = store
                .queue_propose(
                    &QueueCaller::Owner,
                    ProposeItem {
                        title: title.into(),
                        scope: Some("M2".into()),
                        done_when: Some("done".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
            store
                .queue_admit(&QueueCaller::Owner, &item.label, AdmitItem::default())
                .unwrap();
            assert!(store.queue_next(who).unwrap().claimed.is_some());
        }
        now.fetch_add(25 * 3_600_000, Ordering::SeqCst);
        let ann = store.agent("ann").unwrap().agent_id;
        let swept = store.write(|tx| queue_sweep_tx(tx, Some(&ann))).unwrap();
        assert_eq!(swept.released_claims, ["Q2"], "bob's claim only");
        assert_eq!(store.queue_item("Q1").unwrap().state, QueueState::Claimed);
        let swept = store.write(|tx| queue_sweep_tx(tx, None)).unwrap();
        assert_eq!(swept.released_claims, ["Q1"]);
    }

    /// Q10 T18. The statements [`Store::queue_offer`] adds to a session start and a prompt
    /// run on indexes: none scans `queue_items` or `queue_history`, and none reads
    /// `queue_sessions`.
    #[test]
    fn queue_read_uses_its_indexes() {
        let (clock, _) = crate::store::Clock::manual(1_800_000_000_000);
        let store = Store::open_in_memory(clock).unwrap();
        let ready = ready_sql();
        let statements = [OWN_LIMIT_SQL, HELD_SQL, ready.as_str(), GIVEN_UP_SQL];
        // The four statements above are every queue statement of the read. A checkout may
        // have given this file CRLF line ends.
        let source = include_str!("queue.rs").replace("\r\n", "\n");
        let from = source.find("pub fn queue_offer").unwrap();
        let body = &source[from..from + source[from..].find("\n    }\n").unwrap()];
        for call in ["standing", "first_ready", "given_up_at"] {
            assert!(body.contains(call), "queue_offer no longer calls {call}");
        }
        for word in ["query_row", "prepare", "execute", "queue_sessions"] {
            assert!(
                !body.contains(word),
                "queue_offer has SQL of its own: {word}"
            );
        }
        store
            .read(|conn| {
                for sql in statements {
                    assert!(!sql.contains("queue_sessions"), "{sql}");
                    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
                    let params: Vec<&str> = vec!["x"; stmt.parameter_count()];
                    let plan = stmt
                        .query_map(rusqlite::params_from_iter(params), |r| {
                            r.get::<_, String>(3)
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    assert!(!plan.is_empty(), "{sql}");
                    for step in &plan {
                        assert!(
                            !step.starts_with("SCAN"),
                            "a full scan in {sql:?}: {plan:?}"
                        );
                        assert!(!step.contains("queue_sessions"), "{plan:?}");
                    }
                    assert!(
                        plan.iter().any(|s| s.contains("USING")),
                        "no index in {sql:?}: {plan:?}"
                    );
                }
                Ok(())
            })
            .unwrap();
    }
}
