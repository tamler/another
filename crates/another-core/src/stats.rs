//! Countable evidence that coordination happened: edits the edit hook stopped, holds it took,
//! mail pushed into running sessions (mid-task notices and Herdr wakes), whether pushed mail
//! was then read or answered, and requests claimed and completed.
//!
//! Everything is counted from the event log, so each number traces back to events with a
//! timestamp and the identities involved. `another stats` prints the result.

use std::collections::HashSet;

use rusqlite::params;
use serde::Serialize;
use serde_json::{json, Value};

use crate::activity::Since;
use crate::error::Result;
use crate::identity::resolve_agent;
use crate::store::Store;
use crate::time::fmt_ts;
use crate::wake::WakeVia;

/// Kinds this module writes.
pub const NOTICE_EVENT: &str = "push.notified";
pub const WAKE_EVENT: &str = "push.woke";
pub const BLOCKED_EVENT: &str = "hold.blocked";

/// How many recent items `stats` lists next to the counts.
const MAX_ITEMS: usize = 50;

/// One edit the edit hook stopped.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct BlockedEdit {
    pub at: String,
    /// The agent that was stopped.
    pub agent: String,
    pub project: String,
    pub path: String,
    /// The agent holding it, its project and the held pattern.
    pub holder: String,
    pub holder_project: String,
    pub pattern: String,
}

/// One wake the daemon attempted.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct WakeRecord {
    pub at: String,
    pub agent: String,
    pub via: String,
    pub target: String,
    /// `sent` or `failed`.
    pub outcome: String,
    pub messages: u32,
}

/// Counts for a period. "Pushed" means a mid-task notice or a sent wake; prompt notices
/// are counted separately since they arrive anyway when the agent next gets a prompt.
#[derive(Debug, Clone, Default, Serialize, schemars::JsonSchema)]
pub struct TrialStats {
    pub since: String,
    pub until: String,
    /// Edits the edit hook stopped because another agent held the file.
    pub edits_blocked: u32,
    /// Distinct (agent, holder, path) collisions among those, so retries count once.
    pub collisions: u32,
    /// Holds the edit hook took on files about to be edited.
    pub holds_auto_acquired: u32,
    /// Mid-task notices shown after a tool call.
    pub notices_mid_task: u32,
    /// Notices shown at a prompt.
    pub notices_prompt: u32,
    pub wakes_sent: u32,
    pub wakes_failed: u32,
    /// Distinct (agent, message) pairs delivered by a mid-task notice or a sent wake.
    pub messages_pushed: u32,
    /// Of those, how many the agent then read, acknowledged or replied to.
    pub pushes_acted_on: u32,
    pub requests_created: u32,
    pub requests_claimed: u32,
    pub requests_completed: u32,
    /// Most recent first, at most 50.
    pub blocked: Vec<BlockedEdit>,
    /// Most recent first, at most 50.
    pub wakes: Vec<WakeRecord>,
}

fn ids(message_ids: &[String]) -> Value {
    json!(message_ids)
}

impl Store {
    /// Claim the right to announce `message_ids` to `agent`. In one transaction, each id that
    /// is an unread delivery to the agent and has not been announced by any channel yet is
    /// marked announced; those ids are returned, in the order given. Concurrent hooks and the
    /// daemon therefore never announce the same message twice: whoever claims it shows it.
    ///
    /// `channel` is `post_tool`, `prompt`, `wake` or `session_start`. For `post_tool` and
    /// `prompt` a `push.notified` event with every claimed id is written in the same
    /// transaction (the evidence `another stats` counts); a wake records its own event once
    /// it knows the outcome, and the session-start digest is not a push.
    pub fn claim_announcement(
        &self,
        agent: &str,
        message_ids: &[String],
        channel: &str,
        client: Option<&str>,
    ) -> Result<Vec<String>> {
        if message_ids.is_empty() {
            return Ok(Vec::new());
        }
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let mut claimed = Vec::new();
            for id in message_ids {
                let n = tx.execute(
                    "INSERT OR IGNORE INTO announcements (agent_id, message_id, channel, announced_at)
                     SELECT d.agent_id, d.message_id, ?3, ?4 FROM deliveries d
                     WHERE d.agent_id = ?1 AND d.message_id = ?2 AND d.read_at IS NULL",
                    params![me.id, id, channel, tx.now],
                )?;
                if n > 0 {
                    claimed.push(id.clone());
                }
            }
            if !claimed.is_empty() && matches!(channel, "post_tool" | "prompt") {
                tx.emit(
                    &me.address,
                    NOTICE_EVENT,
                    json!({
                        "agent_id": me.id,
                        "agent": me.address,
                        "channel": channel,
                        "client": client,
                        "message_ids": ids(&claimed),
                    }),
                )?;
            }
            Ok(claimed)
        })
    }

    /// Give back a claim (a wake that could not be delivered), so another channel can
    /// announce those messages.
    pub fn release_announcement(&self, agent: &str, message_ids: &[String]) -> Result<()> {
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, agent)?;
            for id in message_ids {
                tx.execute(
                    "DELETE FROM announcements WHERE agent_id = ?1 AND message_id = ?2",
                    params![me.id, id],
                )?;
            }
            Ok(())
        })
    }

    /// Record a wake the daemon attempted for `agent`: `outcome` is `sent` or `failed`.
    pub fn record_wake(
        &self,
        agent: &str,
        via: WakeVia,
        target: &str,
        outcome: &str,
        detail: &str,
        message_ids: &[String],
    ) -> Result<()> {
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, agent)?;
            tx.emit(
                &me.address,
                WAKE_EVENT,
                json!({
                    "agent_id": me.id,
                    "agent": me.address,
                    "via": via.as_str(),
                    "target": target,
                    "outcome": outcome,
                    "detail": detail,
                    "message_ids": ids(message_ids),
                }),
            )?;
            Ok(())
        })
    }

    /// Count this node's coordination evidence since `since` (default: everything).
    pub fn trial_stats(&self, since: Option<Since>) -> Result<TrialStats> {
        self.read(|conn| {
            let (after_seq, after_ts): (i64, i64) = match &since {
                None => (0, i64::MIN),
                Some(Since::Time(t)) => (0, *t),
                Some(Since::After(id)) => (
                    conn.query_row("SELECT seq FROM events WHERE event_id = ?1", [id], |r| {
                        r.get(0)
                    })?,
                    i64::MIN,
                ),
            };
            let mut stmt = conn.prepare(
                "SELECT ts, actor, kind, payload FROM events
                 WHERE node_id = ?1 AND seq > ?2 AND ts >= ?3
                   AND kind IN ('hold.blocked', 'hold.acquired', 'hold.updated',
                                'push.notified', 'push.woke', 'request.created',
                                'request.claimed', 'request.completed')
                 ORDER BY seq DESC",
            )?;
            let rows = stmt
                .query_map(params![self.node_id(), after_seq, after_ts], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut out = TrialStats {
                since: match &since {
                    None => "the beginning".into(),
                    Some(Since::Time(t)) => fmt_ts(*t),
                    Some(Since::After(id)) => format!("after event {id}"),
                },
                until: fmt_ts(self.now()),
                ..Default::default()
            };
            let mut collisions = HashSet::new();
            // (agent_id, message_id, pushed at)
            let mut pushed: Vec<(String, String, i64)> = Vec::new();
            for (ts, actor, kind, payload) in rows {
                let p: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
                let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                let message_ids = || -> Vec<String> {
                    p.get("message_ids")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default()
                };
                match kind.as_str() {
                    "hold.blocked" => {
                        out.edits_blocked += 1;
                        let conflicts = p
                            .get("conflicts")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        for c in &conflicts {
                            let c = |k: &str| {
                                c.get(k).and_then(Value::as_str).unwrap_or("").to_string()
                            };
                            collisions.insert((actor.clone(), c("holder"), c("path")));
                            if out.blocked.len() < MAX_ITEMS {
                                out.blocked.push(BlockedEdit {
                                    at: fmt_ts(ts),
                                    agent: actor.clone(),
                                    project: s("project"),
                                    path: c("path"),
                                    holder: c("holder"),
                                    holder_project: c("project"),
                                    pattern: c("pattern"),
                                });
                            }
                        }
                    }
                    "hold.acquired" | "hold.updated" => {
                        if p.get("auto").and_then(Value::as_bool) == Some(true)
                            && kind == "hold.acquired"
                        {
                            out.holds_auto_acquired += 1;
                        }
                    }
                    "push.notified" => match s("channel").as_str() {
                        "post_tool" => {
                            out.notices_mid_task += 1;
                            let agent_id = s("agent_id");
                            for m in message_ids() {
                                pushed.push((agent_id.clone(), m, ts));
                            }
                        }
                        _ => out.notices_prompt += 1,
                    },
                    "push.woke" => {
                        let messages = message_ids();
                        if s("outcome") == "sent" {
                            out.wakes_sent += 1;
                            let agent_id = s("agent_id");
                            for m in &messages {
                                pushed.push((agent_id.clone(), m.clone(), ts));
                            }
                        } else {
                            out.wakes_failed += 1;
                        }
                        if out.wakes.len() < MAX_ITEMS {
                            out.wakes.push(WakeRecord {
                                at: fmt_ts(ts),
                                agent: actor.clone(),
                                via: s("via"),
                                target: s("target"),
                                outcome: s("outcome"),
                                messages: messages.len() as u32,
                            });
                        }
                    }
                    "request.created" => out.requests_created += 1,
                    "request.claimed" => out.requests_claimed += 1,
                    "request.completed" => out.requests_completed += 1,
                    _ => {}
                }
            }
            out.collisions = collisions.len() as u32;
            // A message pushed more than once counts once, from its first push.
            pushed.sort_by_key(|(_, _, ts)| *ts);
            let mut seen = HashSet::new();
            for (agent_id, message_id, at) in pushed {
                if !seen.insert((agent_id.clone(), message_id.clone())) {
                    continue;
                }
                out.messages_pushed += 1;
                let acted: bool = conn.query_row(
                    "SELECT EXISTS (
                       SELECT 1 FROM deliveries WHERE message_id = ?1 AND agent_id = ?2
                         AND (read_at >= ?3 OR acked_at >= ?3))
                     OR EXISTS (
                       SELECT 1 FROM messages WHERE reply_to = ?1 AND sender_id = ?2
                         AND created_at >= ?3)",
                    params![message_id, agent_id, at],
                    |r| r.get(0),
                )?;
                if acted {
                    out.pushes_acted_on += 1;
                }
            }
            Ok(out)
        })
    }
}
