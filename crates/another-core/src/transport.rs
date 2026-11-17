//! Transport sessions: what Another knows about each agent session running inside a
//! transport (a tool that runs agent sessions, such as Herdr). One row per (agent, client
//! session id) in `transport_sessions`.
//!
//! Three writers, each with its own columns: local hooks create the row and stamp it; the
//! daemon's sweep writes what the transport reports ([`Store::observe_transport_sessions`]);
//! whoever makes a wake call writes its outcome ([`Store::record_transport_wake`]). The CLI
//! crate owns the adapters; this module only stores and decides.

use std::path::{Component, Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::identity::resolve_agent;
use crate::store::{Store, Tx};

/// Written for every wake call (replaces `push.woke`).
pub const TRANSPORT_WAKE_EVENT: &str = "transport.wake";
/// Written when a gone session is confirmed.
pub const TRANSPORT_GONE_EVENT: &str = "transport.gone";

/// How long a session must stay `gone`, under watch, before it is confirmed.
pub const GONE_CONFIRM_AFTER_MS: i64 = 15 * 60_000;
/// A gap between two observations longer than this was not watched time.
const WATCH_GAP_MS: i64 = 2 * 60_000;
/// `gone` enters attention after this long.
const GONE_ATTENTION_MS: i64 = 60_000;
/// `unknown` and `no_answer` enter attention after this long.
const UNCLEAR_ATTENTION_MS: i64 = 10 * 60_000;
/// This many undelivered wakes in a row put the row in attention and slow its wakes.
pub const UNDELIVERED_LIMIT: i64 = 3;
const DAY_MS: i64 = 24 * 3_600_000;

/// What a transport says about one target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportState {
    /// A turn is running.
    Working,
    /// Ready for input.
    Idle,
    /// Waiting on a person at an approval or question dialog.
    Blocked,
    /// Something is at the target and the transport reports no state.
    Present,
    /// The transport sees an agent and cannot classify it.
    Unknown,
    /// The transport answered, and nothing is at the target.
    Gone,
    /// The transport could not be asked.
    NoAnswer,
}

impl TransportState {
    pub fn as_str(self) -> &'static str {
        match self {
            TransportState::Working => "working",
            TransportState::Idle => "idle",
            TransportState::Blocked => "blocked",
            TransportState::Present => "present",
            TransportState::Unknown => "unknown",
            TransportState::Gone => "gone",
            TransportState::NoAnswer => "no_answer",
        }
    }
}

/// One answer about a target: its state and what identifies the session there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Observation {
    pub state: TransportState,
    /// The session the transport says is there.
    pub session_id: Option<String>,
    /// The client kind it names (`claude`, `codex`, ...).
    pub program: Option<String>,
    /// The pane's working directory.
    pub cwd: Option<String>,
    pub detail: String,
}

/// One entry of a transport's listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Session {
    pub target: String,
    pub observation: Observation,
}

/// Yes, no, or the transport could not say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tri {
    Yes,
    No,
    Unknown,
}

impl Tri {
    pub fn as_str(self) -> &'static str {
        match self {
            Tri::Yes => "yes",
            Tri::No => "no",
            Tri::Unknown => "unknown",
        }
    }
}

/// How a wake call ended. This is the only definition of a failed wake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeOutcome {
    /// The transport saw the agent working or at a dialog after the line.
    Started,
    /// The line was submitted and the transport saw no activity. A failure.
    NotStarted,
    /// Submitted; the transport could not say what followed.
    Unconfirmed,
    /// Refused before anything was sent: the agent is waiting on a person.
    RefusedBlocked,
    /// The transport says there is no such session.
    Gone,
    /// Nothing was sent. A failure.
    Undelivered,
    /// An unreadable answer or a deadline; the line may be in the terminal. A failure.
    Unknown,
}

impl WakeOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            WakeOutcome::Started => "started",
            WakeOutcome::NotStarted => "not_started",
            WakeOutcome::Unconfirmed => "unconfirmed",
            WakeOutcome::RefusedBlocked => "refused_blocked",
            WakeOutcome::Gone => "gone",
            WakeOutcome::Undelivered => "undelivered",
            WakeOutcome::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Option<WakeOutcome> {
        Some(match s {
            "started" => WakeOutcome::Started,
            "not_started" => WakeOutcome::NotStarted,
            "unconfirmed" => WakeOutcome::Unconfirmed,
            "refused_blocked" => WakeOutcome::RefusedBlocked,
            "gone" => WakeOutcome::Gone,
            "undelivered" => WakeOutcome::Undelivered,
            "unknown" => WakeOutcome::Unknown,
            _ => return None,
        })
    }

    pub fn delivered(self) -> Tri {
        match self {
            WakeOutcome::Started | WakeOutcome::NotStarted | WakeOutcome::Unconfirmed => Tri::Yes,
            WakeOutcome::RefusedBlocked | WakeOutcome::Gone | WakeOutcome::Undelivered => Tri::No,
            WakeOutcome::Unknown => Tri::Unknown,
        }
    }

    pub fn started(self) -> Tri {
        match self {
            WakeOutcome::Started => Tri::Yes,
            WakeOutcome::Unconfirmed | WakeOutcome::Unknown => Tri::Unknown,
            _ => Tri::No,
        }
    }

    pub fn failure(self) -> bool {
        matches!(
            self,
            WakeOutcome::NotStarted | WakeOutcome::Undelivered | WakeOutcome::Unknown
        )
    }

    /// What the owner is told.
    pub fn meaning(self) -> &'static str {
        match self {
            WakeOutcome::Started => "delivered, the agent started working",
            WakeOutcome::NotStarted => "delivered, did not start",
            WakeOutcome::Unconfirmed => "delivered, not confirmed",
            WakeOutcome::RefusedBlocked => "not sent: the agent is waiting on a person",
            WakeOutcome::Gone => "not sent: no such session",
            WakeOutcome::Undelivered => "not sent",
            WakeOutcome::Unknown => "no readable answer; the line may be in the terminal",
        }
    }

    /// Whether the announcement claim goes back to the hooks.
    fn releases_claim(self) -> bool {
        !matches!(self, WakeOutcome::Started | WakeOutcome::Unconfirmed)
    }
}

/// The kind of thing a wake announces. The queue adds its own kinds later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeKind {
    /// Unannounced mail, claimed in `announcements`.
    Mail,
}

/// One wake call to record.
#[derive(Debug, Clone)]
pub struct TransportWake {
    pub agent: String,
    /// The session row the caller chose, when it chose one.
    pub session_id: Option<String>,
    pub transport: String,
    pub target: String,
    pub outcome: WakeOutcome,
    pub detail: String,
    pub kind: WakeKind,
    /// The announcements claimed for this wake (empty for a manual wake).
    pub message_ids: Vec<String>,
    /// `daemon`, `manual` or `all`.
    pub trigger: String,
}

/// One row of `transport_sessions`. Times are milliseconds since the Unix epoch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TransportSession {
    pub agent_id: String,
    pub agent: String,
    /// The client's session id; empty when there is none.
    pub session_id: String,
    pub transport: String,
    pub target: String,
    pub hook_client: Option<String>,
    pub hook_cwd: Option<String>,
    /// Last local hook call of this session; `None` for a row only a wake made.
    pub hook_seen_at: Option<i64>,
    pub state: Option<String>,
    pub detail: Option<String>,
    pub state_since: Option<i64>,
    pub observed_at: Option<i64>,
    /// Last observation: 0 not matched, 1 by session id, 2 weakly.
    pub matched: i64,
    pub seen_live_at: Option<i64>,
    pub last_alive_at: Option<i64>,
    pub first_gone_at: Option<i64>,
    pub gone_confirmed_at: Option<i64>,
    pub gone_event_id: Option<String>,
    /// `not_started` or `unknown` after a wake that may have typed its line.
    pub wake_stop: Option<String>,
    pub wake_stop_at: Option<i64>,
    pub undelivered_count: i64,
    pub last_wake_outcome: Option<String>,
    pub last_wake_at: Option<i64>,
    pub attention: Option<String>,
    pub attention_since: Option<i64>,
}

impl TransportSession {
    /// A hook of this session made the row (as opposed to a wake to a hand-set target).
    pub fn hook_made(&self) -> bool {
        self.hook_seen_at.is_some()
    }

    /// The stop still holds: none is set, or a hook of this session ran after it.
    pub fn stopped(&self) -> bool {
        self.wake_stop.is_some() && self.hook_seen_at <= self.wake_stop_at
    }

    fn matched_as(&self, states: &[&str]) -> bool {
        self.matched > 0 && self.state.as_deref().is_some_and(|s| states.contains(&s))
    }
}

/// A session confirmed gone: the signal the queue reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfirmedGone {
    pub confirmed_at: i64,
    pub first_gone_at: i64,
    /// The `transport.gone` event.
    pub event_id: String,
    pub transport: String,
    pub target: String,
    pub seen_live_at: i64,
}

/// One row looked at by [`Store::prune_transport_sessions`].
#[derive(Debug, Clone, Serialize)]
pub struct PrunedSession {
    pub agent: String,
    pub session_id: String,
    pub transport: String,
    pub target: String,
    pub pruned: bool,
    pub reason: String,
}

/// What the owner sees for a row: `stuck` after a wake that did not start work, else the
/// observed state, `not observed` before the first sweep.
pub fn shown_state(row: &TransportSession) -> String {
    let Some(state) = row.state.as_deref() else {
        return if row.wake_stop.is_some() {
            "stuck".into()
        } else {
            "not observed".into()
        };
    };
    if row.wake_stop.is_some() && state != "gone" && !row.matched_as(&["working", "blocked"]) {
        return "stuck".into();
    }
    state.to_string()
}

/// Why a row needs the owner, if it does. The only definition; first match wins.
pub fn attention_reason(row: &TransportSession, now: i64) -> Option<&'static str> {
    let state = row.state.as_deref();
    let since = now - row.state_since.unwrap_or(now);
    if state == Some("blocked") {
        return Some("blocked");
    }
    if shown_state(row) == "stuck" {
        return Some("stuck");
    }
    match state {
        Some("gone") if row.gone_confirmed_at.is_none() && since >= GONE_ATTENTION_MS => {
            return Some("gone")
        }
        Some("unknown") if since >= UNCLEAR_ATTENTION_MS => return Some("unknown"),
        Some("no_answer") if since >= UNCLEAR_ATTENTION_MS => return Some("no_answer"),
        _ => {}
    }
    (row.undelivered_count >= UNDELIVERED_LIMIT).then_some("undelivered")
}

/// A directory as hooks and sweeps compare it: symlinks resolved when it exists, else
/// normalised lexically (`.` dropped, `..` applied).
pub fn resolve_dir(path: &Path) -> String {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real.to_string_lossy().into_owned();
    }
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out.to_string_lossy().into_owned()
}

/// A session id as part of a file name: itself when short, else its SHA-256. The caller
/// still encodes the result; it is never used as a path as given.
pub fn session_file_key(session_id: &str) -> String {
    if session_id.chars().count() <= 64 {
        session_id.to_string()
    } else {
        format!("{:x}", Sha256::digest(session_id.as_bytes()))
    }
}

/// How what a transport reports at a row's target relates to the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    /// The transport reports the row's session id.
    ById,
    /// No session id; the client kind and directory are the ones this session's hook saw,
    /// and no newer session of that client kind uses the target.
    Weak,
    /// Not this session. The text says why; empty when the transport named no program.
    No(&'static str),
}

const ANOTHER_SESSION: &str = "the pane hosts another session";
const NEWER_SESSION: &str = "a newer session uses this pane";
const DIFFERS: &str = "no session id, and the client or directory differs";

/// Whether `row` has the newest hook stamp of all rows, of any agent, with its transport,
/// target and client kind.
pub fn newest_at_target(row: &TransportSession, rows: &[TransportSession]) -> bool {
    let key = |r: &TransportSession| (r.hook_seen_at, r.agent_id.clone(), r.session_id.clone());
    !rows.iter().any(|o| {
        o.hook_made()
            && o.transport == row.transport
            && o.target == row.target
            && o.hook_client == row.hook_client
            && key(o) > key(row)
    })
}

/// Does the observation at `row`'s target identify `row`? `newest` is
/// [`newest_at_target`] for the row.
pub fn identify(row: &TransportSession, at_target: &Observation, newest: bool) -> Identity {
    if let Some(id) = &at_target.session_id {
        return if !row.session_id.is_empty() && *id == row.session_id {
            Identity::ById
        } else {
            Identity::No(ANOTHER_SESSION)
        };
    }
    let Some(program) = &at_target.program else {
        return Identity::No("");
    };
    let same_dir = match (&at_target.cwd, &row.hook_cwd) {
        (Some(pane), Some(hook)) => resolve_dir(Path::new(pane)) == *hook,
        _ => false,
    };
    if row.hook_client.as_deref() != Some(program.as_str()) || !same_dir {
        Identity::No(DIFFERS)
    } else if !newest {
        Identity::No(NEWER_SESSION)
    } else {
        Identity::Weak
    }
}

const COLUMNS: &str = "s.agent_id, a.address, s.session_id, s.transport, s.target, s.hook_client,
    s.hook_cwd, s.hook_seen_at, s.state, s.detail, s.state_since, s.observed_at, s.matched,
    s.seen_live_at, s.last_alive_at, s.first_gone_at, s.gone_confirmed_at, s.gone_event_id,
    s.wake_stop, s.wake_stop_at, s.undelivered_count, s.last_wake_outcome, s.last_wake_at,
    s.attention, s.attention_since";

fn load(
    conn: &Connection,
    filter: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<Vec<TransportSession>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM transport_sessions s JOIN agents a ON a.agent_id = s.agent_id
         WHERE {filter} ORDER BY a.address, s.session_id"
    ))?;
    let rows = stmt.query_map(args, |r| {
        Ok(TransportSession {
            agent_id: r.get(0)?,
            agent: r.get(1)?,
            session_id: r.get(2)?,
            transport: r.get(3)?,
            target: r.get(4)?,
            hook_client: r.get(5)?,
            hook_cwd: r.get(6)?,
            hook_seen_at: r.get(7)?,
            state: r.get(8)?,
            detail: r.get(9)?,
            state_since: r.get(10)?,
            observed_at: r.get(11)?,
            matched: r.get(12)?,
            seen_live_at: r.get(13)?,
            last_alive_at: r.get(14)?,
            first_gone_at: r.get(15)?,
            gone_confirmed_at: r.get(16)?,
            gone_event_id: r.get(17)?,
            wake_stop: r.get(18)?,
            wake_stop_at: r.get(19)?,
            undelivered_count: r.get(20)?,
            last_wake_outcome: r.get(21)?,
            last_wake_at: r.get(22)?,
            attention: r.get(23)?,
            attention_since: r.get(24)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Write every column a sweep or a wake may change.
fn save(conn: &Connection, row: &TransportSession) -> Result<()> {
    conn.execute(
        "UPDATE transport_sessions SET transport = ?3, target = ?4, state = ?5, detail = ?6,
           state_since = ?7, observed_at = ?8, matched = ?9, seen_live_at = ?10,
           last_alive_at = ?11, first_gone_at = ?12, gone_confirmed_at = ?13,
           gone_event_id = ?14, wake_stop = ?15, wake_stop_at = ?16, undelivered_count = ?17,
           last_wake_outcome = ?18, last_wake_at = ?19, attention = ?20, attention_since = ?21
         WHERE agent_id = ?1 AND session_id = ?2",
        params![
            row.agent_id,
            row.session_id,
            row.transport,
            row.target,
            row.state,
            row.detail,
            row.state_since,
            row.observed_at,
            row.matched,
            row.seen_live_at,
            row.last_alive_at,
            row.first_gone_at,
            row.gone_confirmed_at,
            row.gone_event_id,
            row.wake_stop,
            row.wake_stop_at,
            row.undelivered_count,
            row.last_wake_outcome,
            row.last_wake_at,
            row.attention,
            row.attention_since,
        ],
    )?;
    Ok(())
}

fn set_attention(row: &mut TransportSession, now: i64) {
    let reason = attention_reason(row, now).map(str::to_string);
    if reason != row.attention {
        row.attention_since = reason.as_ref().map(|_| now);
        row.attention = reason;
    }
}

/// The session's confirmed-gone record, if it has one that still stands. Readable inside a
/// write transaction.
pub(crate) fn gone_confirmed(
    conn: &Connection,
    agent_id: &str,
    session_id: &str,
) -> Result<Option<ConfirmedGone>> {
    if session_id.is_empty() {
        return Ok(None);
    }
    Ok(conn
        .query_row(
            "SELECT gone_confirmed_at, first_gone_at, gone_event_id, transport, target, seen_live_at
             FROM transport_sessions
             WHERE agent_id = ?1 AND session_id = ?2 AND gone_confirmed_at IS NOT NULL
               AND first_gone_at IS NOT NULL AND gone_event_id IS NOT NULL
               AND seen_live_at IS NOT NULL
               AND (hook_seen_at IS NULL OR hook_seen_at <= gone_confirmed_at)",
            params![agent_id, session_id],
            |r| {
                Ok(ConfirmedGone {
                    confirmed_at: r.get(0)?,
                    first_gone_at: r.get(1)?,
                    event_id: r.get(2)?,
                    transport: r.get(3)?,
                    target: r.get(4)?,
                    seen_live_at: r.get(5)?,
                })
            },
        )
        .optional()?)
}

/// When the session was last matched by session id with no stop set. Readable inside a
/// write transaction.
pub(crate) fn last_alive(
    conn: &Connection,
    agent_id: &str,
    session_id: &str,
) -> Result<Option<i64>> {
    if session_id.is_empty() {
        return Ok(None);
    }
    Ok(conn
        .query_row(
            "SELECT last_alive_at FROM transport_sessions WHERE agent_id = ?1 AND session_id = ?2",
            params![agent_id, session_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
}

/// What one listing says about one row: (state, detail, matched, target to follow).
fn classify(
    row: &TransportSession,
    rows: &[TransportSession],
    listed: &[Session],
) -> (TransportState, String, i64, Option<String>) {
    if !row.session_id.is_empty() {
        let same: Vec<&Session> = listed
            .iter()
            .filter(|s| s.observation.session_id.as_deref() == Some(row.session_id.as_str()))
            .collect();
        if let Some(first) = same.first() {
            let pick = same
                .iter()
                .find(|s| s.target == row.target)
                .unwrap_or(first);
            let detail = if same.len() > 1 {
                format!(
                    "{} panes report this session id; using {}",
                    same.len(),
                    pick.target
                )
            } else {
                pick.observation.detail.clone()
            };
            return (pick.observation.state, detail, 1, Some(pick.target.clone()));
        }
    }
    let Some(here) = listed.iter().find(|s| s.target == row.target) else {
        return (
            TransportState::Gone,
            "not in the transport's list".into(),
            0,
            None,
        );
    };
    match identify(row, &here.observation, newest_at_target(row, rows)) {
        // The row's own id is listed only here when the row has none to compare.
        Identity::ById => (
            here.observation.state,
            here.observation.detail.clone(),
            1,
            None,
        ),
        Identity::Weak => (
            here.observation.state,
            here.observation.detail.clone(),
            2,
            None,
        ),
        Identity::No("") => (
            here.observation.state,
            here.observation.detail.clone(),
            0,
            None,
        ),
        Identity::No(why) => (TransportState::Unknown, why.into(), 0, None),
    }
}

impl Store {
    /// A local hook of a session ran inside a transport: make sure its row exists and stamp
    /// it. `session_id` is empty when the client sends none. A row with a session id keeps
    /// the transport and target it was created with (after a pane move the environment still
    /// holds the old pane id; the sweep follows the session instead). The agent's one row
    /// without a session id takes the newest capture.
    pub fn stamp_transport_session(
        &self,
        agent: &str,
        session_id: &str,
        transport: &str,
        target: &str,
        client: &str,
        cwd: &str,
    ) -> Result<()> {
        crate::wake::validate_target(target)?;
        if transport.is_empty() {
            return Err(Error::Invalid("transport name must not be empty".into()));
        }
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let existing: Option<(String, String)> = tx
                .query_row(
                    "SELECT transport, target FROM transport_sessions
                     WHERE agent_id = ?1 AND session_id = ?2",
                    params![me.id, session_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            match existing {
                None => {
                    tx.execute(
                        "INSERT INTO transport_sessions
                           (agent_id, session_id, transport, target, hook_client, hook_cwd, hook_seen_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![me.id, session_id, transport, target, client, cwd, tx.now],
                    )?;
                }
                Some((old_transport, old_target)) => {
                    if session_id.is_empty() && (old_transport != transport || old_target != target)
                    {
                        // Another place: what was observed there says nothing about here.
                        tx.execute(
                            "UPDATE transport_sessions SET transport = ?2, target = ?3,
                               state = NULL, detail = NULL, state_since = NULL, matched = 0
                             WHERE agent_id = ?1 AND session_id = ''",
                            params![me.id, transport, target],
                        )?;
                    }
                    tx.execute(
                        "UPDATE transport_sessions SET hook_client = ?3, hook_cwd = ?4, hook_seen_at = ?5
                         WHERE agent_id = ?1 AND session_id = ?2",
                        params![me.id, session_id, client, cwd, tx.now],
                    )?;
                }
            }
            Ok(())
        })
    }

    /// The session-end hook: delete that session's row only. The row without a session id
    /// goes only when a hook made it and it still names `place` (transport, target).
    pub fn end_transport_session(
        &self,
        agent: &str,
        session_id: &str,
        place: Option<(&str, &str)>,
    ) -> Result<bool> {
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let n = if session_id.is_empty() {
                let Some((transport, target)) = place else {
                    return Ok(false);
                };
                tx.execute(
                    "DELETE FROM transport_sessions WHERE agent_id = ?1 AND session_id = ''
                       AND hook_seen_at IS NOT NULL AND transport = ?2 AND target = ?3",
                    params![me.id, transport, target],
                )?
            } else {
                tx.execute(
                    "DELETE FROM transport_sessions WHERE agent_id = ?1 AND session_id = ?2",
                    params![me.id, session_id],
                )?
            };
            Ok(n > 0)
        })
    }

    /// `another wake clear`: delete every session row of the agent.
    pub fn clear_transport_sessions(&self, agent: &str) -> Result<usize> {
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, agent)?;
            Ok(tx.execute(
                "DELETE FROM transport_sessions WHERE agent_id = ?1",
                [&me.id],
            )?)
        })
    }

    /// Every session row, by agent address and session id.
    pub fn transport_sessions(&self) -> Result<Vec<TransportSession>> {
        self.read(|conn| load(conn, "1 = 1", &[]))
    }

    /// See [`ConfirmedGone`]. `agent_id` is the agent's id, not its address.
    pub fn transport_gone_confirmed(
        &self,
        agent_id: &str,
        session_id: &str,
    ) -> Result<Option<ConfirmedGone>> {
        self.read(|conn| gone_confirmed(conn, agent_id, session_id))
    }

    /// The session's last-alive time. `agent_id` is the agent's id, not its address.
    pub fn transport_last_alive(&self, agent_id: &str, session_id: &str) -> Result<Option<i64>> {
        self.read(|conn| last_alive(conn, agent_id, session_id))
    }

    /// Apply one whole listing of `transport` to its hook-made rows, in one transaction.
    /// `listing` is the sessions the transport listed, or why it could not be asked.
    /// `confirm_after` is how long a session must stay gone under watch before it is
    /// confirmed (milliseconds); `None` confirms nothing. Returns the sessions confirmed
    /// gone by this call.
    pub fn observe_transport_sessions(
        &self,
        transport: &str,
        listing: std::result::Result<&[Session], &str>,
        confirm_after: Option<i64>,
    ) -> Result<Vec<TransportSession>> {
        self.write(|tx| {
            let mut rows = load(
                tx,
                "s.transport = ?1 AND s.hook_seen_at IS NOT NULL",
                &[&transport],
            )?;
            // Trust is decided first, for the whole listing: at least one row is matched by
            // session id at its stored target and had been seen live before this call.
            let trusted = listing.is_ok_and(|listed| {
                rows.iter().any(|r| {
                    !r.session_id.is_empty()
                        && r.seen_live_at.is_some()
                        && listed.iter().any(|s| {
                            s.target == r.target
                                && s.observation.session_id.as_deref()
                                    == Some(r.session_id.as_str())
                        })
                })
            });
            if listing.is_ok() && !trusted {
                for r in rows.iter_mut().filter(|r| r.gone_confirmed_at.is_none()) {
                    r.seen_live_at = None;
                    r.first_gone_at = None;
                }
            }
            let before = rows.clone();
            let mut confirmed = Vec::new();
            for row in rows.iter_mut() {
                let (state, detail, matched, follow) = match listing {
                    Ok(listed) => classify(row, &before, listed),
                    Err(why) => (TransportState::NoAnswer, why.to_string(), 0, None),
                };
                observe_row(
                    tx,
                    row,
                    (state, detail, matched, follow),
                    trusted,
                    confirm_after,
                )?;
                if row.gone_confirmed_at == Some(tx.now) {
                    confirmed.push(row.clone());
                }
                save(tx, row)?;
            }
            Ok(confirmed)
        })
    }

    /// Record one wake call in one transaction: the `transport.wake` event, the outcome on
    /// every row of the agent with the prompted transport and target, the announcement
    /// claim given back when the outcome says so, and the recomputed attention. Returns the
    /// event id.
    ///
    /// When no row of the agent has that transport and target, the agent's row without a
    /// session id takes the outcome if a wake made it (or none exists yet); one a hook made
    /// is left alone, and the outcome is then in the event only.
    pub fn record_transport_wake(&self, wake: &TransportWake) -> Result<String> {
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, &wake.agent)?;
            let outcome = wake.outcome;
            let event_id = tx.emit(
                &me.address,
                TRANSPORT_WAKE_EVENT,
                json!({
                    "agent_id": me.id,
                    "agent": me.address,
                    "session_id": wake.session_id,
                    "transport": wake.transport,
                    "target": wake.target,
                    "outcome": outcome.as_str(),
                    "delivered": outcome.delivered().as_str(),
                    "started": outcome.started().as_str(),
                    "failure": outcome.failure(),
                    "detail": wake.detail,
                    "message_ids": wake.message_ids,
                    "trigger": wake.trigger,
                }),
            )?;
            if wake.kind == WakeKind::Mail && outcome.releases_claim() {
                crate::stats::release_announcement_tx(tx, &me.id, &wake.message_ids)?;
            }
            let mut rows = load(
                tx,
                "s.agent_id = ?1 AND s.transport = ?2 AND s.target = ?3",
                &[&me.id, &wake.transport, &wake.target],
            )?;
            if rows.is_empty() {
                let bare = load(tx, "s.agent_id = ?1 AND s.session_id = ''", &[&me.id])?;
                match bare.first() {
                    Some(row) if row.hook_made() => {}
                    Some(_) => {
                        tx.execute(
                            "UPDATE transport_sessions SET transport = ?2, target = ?3
                             WHERE agent_id = ?1 AND session_id = ''",
                            params![me.id, wake.transport, wake.target],
                        )?;
                    }
                    None => {
                        tx.execute(
                            "INSERT INTO transport_sessions (agent_id, session_id, transport, target)
                             VALUES (?1, '', ?2, ?3)",
                            params![me.id, wake.transport, wake.target],
                        )?;
                    }
                }
                rows = load(
                    tx,
                    "s.agent_id = ?1 AND s.transport = ?2 AND s.target = ?3",
                    &[&me.id, &wake.transport, &wake.target],
                )?;
            }
            for row in rows.iter_mut() {
                row.last_wake_outcome = Some(outcome.as_str().to_string());
                row.last_wake_at = Some(tx.now);
                match outcome {
                    WakeOutcome::Started => {
                        row.undelivered_count = 0;
                        row.wake_stop = None;
                        row.wake_stop_at = None;
                    }
                    WakeOutcome::NotStarted | WakeOutcome::Unknown => {
                        row.wake_stop = Some(outcome.as_str().to_string());
                        row.wake_stop_at = Some(tx.now);
                    }
                    WakeOutcome::Undelivered => row.undelivered_count += 1,
                    WakeOutcome::Unconfirmed
                    | WakeOutcome::RefusedBlocked
                    | WakeOutcome::Gone => {}
                }
                set_attention(row, tx.now);
                save(tx, row)?;
            }
            Ok(event_id)
        })
    }

    /// Row lifetime: drop rows confirmed gone more than a day ago, rows whose hooks have
    /// been quiet for a day unless the transport still matched them inside that day, and
    /// rows only a wake made whose last wake is more than a day old. Asks no transport.
    pub fn prune_transport_sessions(&self, dry_run: bool) -> Result<Vec<PrunedSession>> {
        self.write(|tx| {
            let mut out = Vec::new();
            for row in load(tx, "1 = 1", &[])? {
                let old = |at: Option<i64>| at.is_none_or(|t| tx.now - t > DAY_MS);
                let reason = if row.gone_confirmed_at.is_some_and(|t| tx.now - t > DAY_MS) {
                    Some("confirmed gone over a day ago")
                } else if !row.hook_made() {
                    old(row.last_wake_at).then_some("no wake for over a day")
                } else if old(row.hook_seen_at) && (old(row.observed_at) || row.matched == 0) {
                    Some("no hook for over a day, and the transport does not show the session")
                } else {
                    None
                };
                if reason.is_some() && !dry_run {
                    tx.execute(
                        "DELETE FROM transport_sessions WHERE agent_id = ?1 AND session_id = ?2",
                        params![row.agent_id, row.session_id],
                    )?;
                }
                out.push(PrunedSession {
                    agent: row.agent,
                    session_id: row.session_id,
                    transport: row.transport,
                    target: row.target,
                    pruned: reason.is_some(),
                    reason: reason.unwrap_or("kept").to_string(),
                });
            }
            Ok(out)
        })
    }
}

/// Steps 1 to 6 for one row and one listing result.
fn observe_row(
    tx: &Tx<'_>,
    row: &mut TransportSession,
    (state, detail, matched, follow): (TransportState, String, i64, Option<String>),
    trusted: bool,
    confirm_after: Option<i64>,
) -> Result<()> {
    let now = tx.now;
    // 1. The observation itself. A gap in the watch restarts the gone window.
    if row.state.as_deref() != Some(state.as_str()) {
        row.state = Some(state.as_str().to_string());
        row.state_since = Some(now);
    }
    row.detail = Some(detail);
    row.matched = matched;
    if row.observed_at.is_some_and(|at| now - at > WATCH_GAP_MS) {
        row.first_gone_at = None;
    }
    row.observed_at = Some(now);
    if let Some(target) = follow {
        row.target = target;
    }
    // 2. A stop ends when the agent is seen working or at a dialog, or its own hook ran.
    if row.matched_as(&["working", "blocked"]) || !row.stopped() {
        row.wake_stop = None;
        row.wake_stop_at = None;
    }
    // A hook of this session after the confirmation: it is alive.
    if row
        .gone_confirmed_at
        .is_some_and(|at| row.hook_seen_at > Some(at))
    {
        row.gone_confirmed_at = None;
        row.gone_event_id = None;
    }
    // 3. Only a match by session id is a sighting.
    if matched == 1 {
        row.seen_live_at.get_or_insert(now);
        if row.wake_stop.is_none() {
            row.last_alive_at = Some(now);
        }
        row.gone_confirmed_at = None;
        row.gone_event_id = None;
    }
    // 4 and 5. The gone window, and its confirmation.
    if state != TransportState::Gone {
        row.first_gone_at = None;
    } else {
        match row.first_gone_at {
            None => row.first_gone_at = Some(now),
            Some(first) if row.hook_seen_at > Some(first) => row.first_gone_at = Some(now),
            Some(first) => {
                let due = confirm_after.is_some_and(|after| now - first >= after);
                if due && trusted && row.seen_live_at.is_some() && row.gone_confirmed_at.is_none() {
                    let event_id = tx.emit(
                        &row.agent,
                        TRANSPORT_GONE_EVENT,
                        json!({
                            "agent_id": row.agent_id,
                            "agent": row.agent,
                            "session_id": row.session_id,
                            "transport": row.transport,
                            "target": row.target,
                            "seen_live_at": row.seen_live_at,
                            "first_gone_at": first,
                            "confirmed_at": now,
                        }),
                    )?;
                    row.gone_confirmed_at = Some(now);
                    row.gone_event_id = Some(event_id);
                }
            }
        }
    }
    // 6.
    set_attention(row, now);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_outcome_table_is_the_definition_of_failure() {
        use WakeOutcome::*;
        let table = [
            (Started, Tri::Yes, Tri::Yes, false),
            (NotStarted, Tri::Yes, Tri::No, true),
            (Unconfirmed, Tri::Yes, Tri::Unknown, false),
            (RefusedBlocked, Tri::No, Tri::No, false),
            (Gone, Tri::No, Tri::No, false),
            (Undelivered, Tri::No, Tri::No, true),
            (Unknown, Tri::Unknown, Tri::Unknown, true),
        ];
        for (outcome, delivered, started, failure) in table {
            assert_eq!(outcome.delivered(), delivered, "{outcome:?}");
            assert_eq!(outcome.started(), started, "{outcome:?}");
            assert_eq!(outcome.failure(), failure, "{outcome:?}");
            assert_eq!(WakeOutcome::parse(outcome.as_str()), Some(outcome));
        }
    }

    #[test]
    fn a_directory_that_does_not_exist_is_normalised_lexically() {
        assert_eq!(resolve_dir(Path::new("/work/app/./src/..")), "/work/app");
    }

    #[test]
    fn a_long_session_id_becomes_its_digest_in_a_file_name() {
        assert_eq!(session_file_key("abc"), "abc");
        let long = "x".repeat(65);
        let key = session_file_key(&long);
        assert_eq!(key.len(), 64);
        assert_ne!(key, long);
    }
}
