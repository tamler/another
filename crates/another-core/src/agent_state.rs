//! Agent state in Herdr's terms (D70): `idle`, `working`, `blocked` (with a message) and
//! `unknown`, derived from rows and events this node already stores. Nothing is written.
//!
//! - `unknown`: the session ended (stored status `offline`), as Herdr says when it has no
//!   signal.
//! - `blocked`: the agent's newest edit-hook outcome in the live window is a stopped edit
//!   (`hold.blocked`, or `hold.waiting`/`hold.wait_failed` for a retry) and at least one
//!   hold that stopped it is still live. A retry inside an unexpired backoff window writes
//!   no event (D62) and is never seen. Only the next attempt after the backoff ends records
//!   one, so nothing bounds how long such a retry stays invisible.
//! - `working`: seen within the live window (the dashboard's `live` presence).
//! - `idle`: an open session that has been quiet longer than that.
//!
//! Another has no turn-end hook, so `working` means recent activity, not a running turn.

use std::collections::HashMap;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::Value;

use crate::dashboard::{presence, LIVE_MS};
use crate::error::Result;
use crate::identity::resolve_agent;
use crate::store::Store;
use crate::time::fmt_opt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Idle,
    Working,
    Blocked,
    Unknown,
}

impl AgentState {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentState::Idle => "idle",
            AgentState::Working => "working",
            AgentState::Blocked => "blocked",
            AgentState::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct AgentStatus {
    pub state: AgentState,
    /// Why the agent is blocked, such as `src/a.rs is held by alice@shop`.
    pub message: Option<String>,
    /// Ids of the live holds that stopped the agent's last edit.
    pub blocked_by: Vec<String>,
}

/// A stopped edit that still stands: its message and the live holds behind it.
#[derive(Debug, Clone)]
pub(crate) struct Stop {
    pub message: String,
    pub holds: Vec<String>,
}

/// Agents of `node` whose newest edit-hook outcome within the live window is a stop with a
/// blocker that is still live, keyed by agent id. With `only`, just that agent.
pub(crate) fn current_stops(
    conn: &Connection,
    node: &str,
    now: i64,
    only: Option<&str>,
) -> Result<HashMap<String, Stop>> {
    let since = now - LIVE_MS;
    // Local events are appended in time order, so the newest event before the window bounds
    // the scan to the events inside it.
    let floor: i64 = conn
        .query_row(
            "SELECT seq FROM events WHERE node_id = ?1 AND ts < ?2 ORDER BY seq DESC LIMIT 1",
            params![node, since],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    let mut stmt = conn.prepare(
        "SELECT kind, payload FROM events
         WHERE node_id = ?1 AND seq > ?2 AND ts >= ?3
           AND kind IN ('hold.blocked', 'hold.waiting', 'hold.wait_failed',
                        'hold.acquired', 'hold.updated')
           AND (?4 IS NULL OR json_extract(payload, '$.agent_id') = ?4)
         ORDER BY seq DESC",
    )?;
    let rows = stmt
        .query_map(params![node, floor, since, only], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut decided: HashMap<String, Option<Stop>> = HashMap::new();
    for (kind, payload) in rows {
        let p: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
        let Some(agent) = p.get("agent_id").and_then(Value::as_str) else {
            continue;
        };
        if decided.contains_key(agent) {
            continue;
        }
        let text = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        // (hold id, path, holder as recorded) for each hold that stopped this attempt.
        let stopped_by: Vec<(String, String, Option<String>)> = match kind.as_str() {
            "hold.blocked" => p
                .get("conflicts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|c| (text(c, "hold_id"), text(c, "path"), Some(text(c, "holder"))))
                .collect(),
            // A retry with unchanged blockers writes no second `hold.blocked` (D62). One
            // that starts a new backoff window, or fails to record its wait, writes one of
            // these, with the blocking hold ids for one path and no holder name.
            "hold.waiting" | "hold.wait_failed" => p
                .get("blockers")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|h| (h.to_string(), text(&p, "path"), None))
                .collect(),
            _ => {
                // Only a hold the edit hook took means the agent went on to edit something.
                if p.get("auto").and_then(Value::as_bool) == Some(true) {
                    decided.insert(agent.to_string(), None);
                }
                continue;
            }
        };
        let mut live: Vec<(String, String, String)> = Vec::new();
        for (hold, path, recorded) in stopped_by {
            let current: Option<String> = conn
                .query_row(
                    "SELECT a.address FROM holds h JOIN agents a ON a.agent_id = h.agent_id
                     WHERE h.hold_id = ?1 AND h.released_at IS NULL AND h.expires_at > ?2",
                    params![hold, now],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(current) = current {
                if !live.iter().any(|(h, _, _)| *h == hold) {
                    live.push((hold, path, recorded.unwrap_or(current)));
                }
            }
        }
        let stop = live.first().map(|(_, path, holder)| {
            // A file name can hold control or bidi characters; the message is shown on the
            // board, the dashboard and Herdr's pane, so it is one safe line (D75).
            let mut message = crate::mail::single_line(&format!("{path} is held by {holder}"), 300);
            if live.len() > 1 {
                message.push_str(&format!(" (+{} more)", live.len() - 1));
            }
            Stop {
                message,
                holds: live.iter().map(|(h, _, _)| h.clone()).collect(),
            }
        });
        decided.insert(agent.to_string(), stop);
    }
    Ok(decided
        .into_iter()
        .filter_map(|(agent, stop)| stop.map(|s| (agent, s)))
        .collect())
}

/// The state of an agent with stored `status` and `last_seen_at`, given its current stop.
pub(crate) fn status_of(
    status: &str,
    last_seen_at: Option<&str>,
    now: i64,
    stop: Option<&Stop>,
) -> AgentStatus {
    let (presence, _) = presence(status, last_seen_at, now);
    // `unknown` means the session ended (stored status), not that an open session has been
    // quiet for hours: dashboard presence calls that `offline` too (R1 P3).
    let ended = status != "active";
    let (state, stop) = match (ended, presence, stop) {
        (true, _, _) => (AgentState::Unknown, None),
        (false, _, Some(stop)) => (AgentState::Blocked, Some(stop)),
        (false, "live", None) => (AgentState::Working, None),
        _ => (AgentState::Idle, None),
    };
    AgentStatus {
        state,
        message: stop.map(|s| s.message.clone()),
        blocked_by: stop.map(|s| s.holds.clone()).unwrap_or_default(),
    }
}

impl Store {
    /// One local agent's state in Herdr's terms (see the module docs). Reads only.
    pub fn agent_status(&self, agent: &str) -> Result<AgentStatus> {
        let now = self.now();
        self.read(|conn| {
            let me = resolve_agent(conn, self.node_id(), agent)?;
            let (status, last_seen): (String, Option<i64>) = conn.query_row(
                "SELECT status, last_seen_at FROM agents WHERE agent_id = ?1",
                [&me.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let stops = current_stops(conn, self.node_id(), now, Some(&me.id))?;
            Ok(status_of(
                &status,
                fmt_opt(last_seen).as_deref(),
                now,
                stops.get(&me.id),
            ))
        })
    }
}
