//! Pages of the event log and the audit log for `another log export`, oldest first.
//!
//! Each page is one short read, keyed on `seq` (or `audit_id`), so an export of any size
//! holds no lock across pages and never repeats or skips a row: a caller asks for the
//! rows after the last key it wrote.

use rusqlite::params;
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::store::Store;
use crate::time::fmt_ts;

/// The rows a page of the event log may include.
#[derive(Debug, Clone, Default)]
pub struct ExportFilter {
    /// Only rows at or after this time (ms since the epoch).
    pub since_ms: Option<i64>,
    /// Only rows at or before this time (ms since the epoch).
    pub until_ms: Option<i64>,
    /// Only these kinds (empty: all).
    pub kinds: Vec<String>,
    /// Only this node's own events, not peers' events stored here.
    pub local_only: bool,
}

/// The largest page.
pub const MAX_EXPORT_PAGE: u32 = 10_000;

impl Store {
    /// The `seq` of a stored event, for "after this event" cursors.
    pub fn event_seq(&self, event_id: &str) -> Result<i64> {
        self.read(|conn| {
            conn.query_row(
                "SELECT seq FROM events WHERE event_id = ?1",
                [event_id.to_ascii_uppercase()],
                |r| r.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    Error::NotFound(format!("no event {event_id} in this log"))
                }
                e => e.into(),
            })
        })
    }

    /// Up to `limit` events with `seq > after_seq` that pass `filter`, oldest first, as
    /// export lines: `seq`, `event_id`, `node_id`, `ts` (RFC 3339), `actor`, `kind`,
    /// `payload` (parsed; kept as a string with `payload_raw: true` when it does not
    /// parse), `prev_id`, `sig`.
    pub fn export_events(
        &self,
        after_seq: i64,
        filter: &ExportFilter,
        limit: u32,
    ) -> Result<Vec<Value>> {
        let limit = limit.clamp(1, MAX_EXPORT_PAGE);
        let kinds = (!filter.kinds.is_empty()).then(|| json!(filter.kinds).to_string());
        let node = filter.local_only.then(|| self.node_id().to_string());
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT seq, event_id, node_id, ts, actor, kind, payload, prev_id, sig
                 FROM events
                 WHERE seq > ?1
                   AND (?2 IS NULL OR ts >= ?2)
                   AND (?3 IS NULL OR ts <= ?3)
                   AND (?4 IS NULL OR kind IN (SELECT value FROM json_each(?4)))
                   AND (?5 IS NULL OR node_id = ?5)
                 ORDER BY seq
                 LIMIT ?6",
            )?;
            let rows = stmt.query_map(
                params![
                    after_seq,
                    filter.since_ms,
                    filter.until_ms,
                    kinds,
                    node,
                    limit
                ],
                |r| {
                    let raw: String = r.get(6)?;
                    let sig: Option<String> = r.get(8)?;
                    let mut line = json!({
                        "seq": r.get::<_, i64>(0)?,
                        "event_id": r.get::<_, String>(1)?,
                        "node_id": r.get::<_, String>(2)?,
                        "ts": fmt_ts(r.get(3)?),
                        "actor": r.get::<_, String>(4)?,
                        "kind": r.get::<_, String>(5)?,
                        "prev_id": r.get::<_, Option<String>>(7)?,
                        "sig": sig.filter(|s| !s.is_empty()),
                    });
                    match serde_json::from_str::<Value>(&raw) {
                        Ok(v) => line["payload"] = v,
                        Err(_) => {
                            line["payload"] = Value::String(raw);
                            line["payload_raw"] = Value::Bool(true);
                        }
                    }
                    Ok(line)
                },
            )?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Up to `limit` audit-log rows of this node with `audit_id > after_id` in the time
    /// window, oldest first, as export lines.
    pub fn export_audit(
        &self,
        after_id: i64,
        since_ms: Option<i64>,
        until_ms: Option<i64>,
        limit: u32,
    ) -> Result<Vec<Value>> {
        let limit = limit.clamp(1, MAX_EXPORT_PAGE);
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT audit_id, ts, token_id, agent, operation, outcome, detail, remote
                 FROM audit_log
                 WHERE node_id = ?1 AND audit_id > ?2
                   AND (?3 IS NULL OR ts >= ?3)
                   AND (?4 IS NULL OR ts <= ?4)
                 ORDER BY audit_id
                 LIMIT ?5",
            )?;
            let rows = stmt.query_map(
                params![self.node_id(), after_id, since_ms, until_ms, limit],
                |r| {
                    Ok(json!({
                        "audit_id": r.get::<_, i64>(0)?,
                        "ts": fmt_ts(r.get(1)?),
                        "token_id": r.get::<_, Option<String>>(2)?,
                        "agent": r.get::<_, Option<String>>(3)?,
                        "operation": r.get::<_, String>(4)?,
                        "outcome": r.get::<_, String>(5)?,
                        "detail": r.get::<_, Option<String>>(6)?,
                        "remote": r.get::<_, Option<String>>(7)?,
                    }))
                },
            )?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }
}
