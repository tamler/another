//! Requests: owned work items opened from a message.
//!
//! Lifecycle: `open → claimed → done`, with `stale` when the due time passes and
//! `cancelled` when the creator withdraws it. A stale request may be claimed by anyone.

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{Error, Result};
use crate::identity::{address, resolve_agent, AgentRef};
use crate::mail::{send_tx, touch, Importance, SendMessage};
use crate::store::{Store, Tx};
use crate::time::{fmt_opt, fmt_ts, later_secs};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    Open,
    Claimed,
    Done,
    Stale,
    Cancelled,
}

impl RequestStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RequestStatus::Open => "open",
            RequestStatus::Claimed => "claimed",
            RequestStatus::Done => "done",
            RequestStatus::Stale => "stale",
            RequestStatus::Cancelled => "cancelled",
        }
    }
}

impl std::str::FromStr for RequestStatus {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "open" => Ok(RequestStatus::Open),
            "claimed" => Ok(RequestStatus::Claimed),
            "done" => Ok(RequestStatus::Done),
            "stale" => Ok(RequestStatus::Stale),
            "cancelled" => Ok(RequestStatus::Cancelled),
            _ => Err(Error::Invalid(format!(
                "unknown request status {s:?}; expected open, claimed, done, stale or cancelled"
            ))),
        }
    }
}

/// What to do with a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RequestAction {
    Claim,
    Release,
    Complete,
    Cancel,
}

impl std::str::FromStr for RequestAction {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "claim" => Ok(RequestAction::Claim),
            "release" => Ok(RequestAction::Release),
            "complete" => Ok(RequestAction::Complete),
            "cancel" => Ok(RequestAction::Cancel),
            _ => Err(Error::Invalid(format!(
                "unknown request action {s:?}; expected claim, release, complete or cancel"
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Request {
    pub request_id: String,
    pub title: String,
    pub status: RequestStatus,
    pub owner: Option<String>,
    pub created_by: String,
    pub thread_id: String,
    pub message_id: String,
    pub due_at: Option<String>,
    pub note: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub claimed_at: Option<String>,
    pub closed_at: Option<String>,
}

/// Longest completion or cancellation note a local caller may give, in bytes (D99).
pub const MAX_REQUEST_NOTE_BYTES: usize = 16 * 1024;

/// Input for [`Store::update_request`].
#[derive(Debug, Clone)]
pub struct RequestUpdate {
    pub agent: String,
    pub request_id: String,
    pub action: RequestAction,
    /// Completion or cancellation note (also mailed to the creator on completion).
    pub note: Option<String>,
    /// On claim: set a new due time this many seconds from now.
    pub due_in_seconds: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct RequestFilter {
    pub status: Option<RequestStatus>,
    /// Owner `name` or `name@project`.
    pub owner: Option<String>,
    pub limit: u32,
}

const REQUEST_SELECT: &str = "SELECT r.request_id, r.title, r.status, o.address, NULL, c.address, NULL,
       r.thread_id, r.message_id, r.due_at, r.note, r.created_at, r.updated_at, r.claimed_at, r.closed_at
     FROM requests r
     JOIN agents c ON c.agent_id = r.created_by
     LEFT JOIN projects cp ON cp.project_id = c.project_id
     LEFT JOIN agents o ON o.agent_id = r.owner_id
     LEFT JOIN projects op ON op.project_id = o.project_id";

fn request_from_row(r: &Row<'_>) -> rusqlite::Result<Request> {
    let status: String = r.get(2)?;
    let owner: Option<String> = r.get(3)?;
    let owner_slug: Option<String> = r.get(4)?;
    let creator: String = r.get(5)?;
    let creator_slug: Option<String> = r.get(6)?;
    Ok(Request {
        request_id: r.get(0)?,
        title: r.get(1)?,
        status: status.parse().unwrap_or(RequestStatus::Open),
        owner: owner.map(|o| address(&o, owner_slug.as_deref())),
        created_by: address(&creator, creator_slug.as_deref()),
        thread_id: r.get(7)?,
        message_id: r.get(8)?,
        due_at: fmt_opt(r.get(9)?),
        note: r.get(10)?,
        created_at: fmt_ts(r.get(11)?),
        updated_at: fmt_ts(r.get(12)?),
        claimed_at: fmt_opt(r.get(13)?),
        closed_at: fmt_opt(r.get(14)?),
    })
}

pub(crate) fn get_request(conn: &Connection, request_id: &str) -> Result<Request> {
    conn.query_row(
        &format!("{REQUEST_SELECT} WHERE r.request_id = ?1"),
        [request_id],
        request_from_row,
    )
    .optional()?
    .ok_or_else(|| Error::NotFound(format!("unknown request {request_id:?}")))
}

fn due_from(now: i64, secs: Option<i64>) -> Result<Option<i64>> {
    match secs {
        None => Ok(None),
        Some(s) if s > 0 => later_secs(now, s, "due time").map(Some),
        Some(_) => Err(Error::Invalid("due_in_seconds must be positive".into())),
    }
}

pub(crate) fn create_request_tx(
    tx: &Tx<'_>,
    request_id: &str,
    creator: &AgentRef,
    message_id: &str,
    thread_id: &str,
    title: &str,
    due_in_seconds: Option<i64>,
) -> Result<()> {
    let due_at = due_from(tx.now, due_in_seconds)?;
    tx.emit(
        &creator.address,
        "request.created",
        json!({
            "request_id": request_id,
            "message_id": message_id,
            "thread_id": thread_id,
            "title": title,
            "due_at": due_at,
        }),
    )?;
    tx.execute(
        "INSERT INTO requests (request_id, node_id, message_id, thread_id, title, created_by,
           status, due_at, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'open', ?7, ?8, ?8)",
        params![request_id, tx.node_id, message_id, thread_id, title, creator.id, due_at, tx.now],
    )?;
    Ok(())
}

/// Mark open/claimed requests past their due time as stale. Returns how many changed.
pub(crate) fn stale_overdue_tx(tx: &Tx<'_>) -> Result<usize> {
    let mut stmt = tx.prepare(
        "SELECT request_id, status, message_id, thread_id, title, created_by, owner_id, due_at
         FROM requests
         WHERE node_id = ?1 AND status IN ('open', 'claimed') AND due_at IS NOT NULL AND due_at <= ?2",
    )?;
    let overdue = stmt
        .query_map(params![tx.node_id, tx.now], |r| {
            Ok((
                r.get::<_, String>(1)?,
                ReminderRow {
                    request_id: r.get(0)?,
                    message_id: r.get(2)?,
                    thread_id: r.get(3)?,
                    title: r.get(4)?,
                    creator_id: r.get(5)?,
                    owner_id: r.get(6)?,
                    due_at: r.get(7)?,
                },
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let remind = reminders_enabled();
    for (from, row) in &overdue {
        tx.emit(
            "system",
            "request.stale",
            json!({ "request_id": row.request_id, "from": from, "reason": "past due" }),
        )?;
        tx.execute(
            "UPDATE requests SET status = 'stale', updated_at = ?2 WHERE request_id = ?1",
            params![row.request_id, tx.now],
        )?;
        if remind {
            send_overdue_notice_tx(tx, row)?;
        }
    }
    Ok(overdue.len())
}

/// The last line of every reminder: it is not written by the agent it comes from.
const AUTOMATIC: &str = "(Automatic notice from Another.)";

/// Shortest and longest time before a deadline that the owner is reminded.
const REMINDER_LEAD_MIN_MS: i64 = 5 * 60 * 1000;
const REMINDER_LEAD_MAX_MS: i64 = 60 * 60 * 1000;

/// Deadline reminders are on unless `ANOTHER_REQUEST_REMINDERS=off` in the process that
/// runs the sweep.
fn reminders_enabled() -> bool {
    !std::env::var("ANOTHER_REQUEST_REMINDERS").is_ok_and(|v| v.trim().eq_ignore_ascii_case("off"))
}

/// How long before `due_at` the owner of a request claimed at `claimed_at` is reminded: a
/// fifth of the claimed span, between five minutes and an hour. `None` when the claim left
/// less than the minimum, so there is no useful moment to remind.
fn reminder_lead_ms(claimed_at: i64, due_at: i64) -> Option<i64> {
    let span = due_at.saturating_sub(claimed_at);
    (span >= REMINDER_LEAD_MIN_MS)
        .then(|| (span / 5).clamp(REMINDER_LEAD_MIN_MS, REMINDER_LEAD_MAX_MS))
}

struct ReminderRow {
    request_id: String,
    message_id: String,
    thread_id: String,
    title: String,
    creator_id: String,
    owner_id: Option<String>,
    due_at: i64,
}

/// One reminder mail: its kind, sender, recipient, text and extra event fields.
struct Reminder<'a> {
    kind: &'a str,
    from_id: &'a str,
    to_id: &'a str,
    subject: String,
    body: String,
    extra: serde_json::Map<String, serde_json::Value>,
}

/// Send one reminder in its own savepoint with its `request.reminded` event. A reminder
/// that cannot be delivered is rolled back and recorded as `request.notify_failed`, and
/// the sweep goes on. A database failure (the savepoint itself, or writing that event)
/// still fails the caller's transaction, as any write would.
fn send_reminder_tx(tx: &Tx<'_>, row: &ReminderRow, reminder: Reminder<'_>) -> Result<()> {
    let notice = SendMessage {
        subject: reminder.subject,
        body: reminder.body,
        reply_to: Some(row.message_id.clone()),
        thread_id: Some(row.thread_id.clone()),
        ..Default::default()
    };
    let sent = crate::holds::attempt(tx, "request_reminder", || {
        let message = crate::mail::send_notice_tx(tx, reminder.from_id, reminder.to_id, &notice)?;
        let mut payload = serde_json::Map::new();
        payload.insert("request_id".into(), json!(row.request_id));
        payload.insert("kind".into(), json!(reminder.kind));
        payload.insert("to".into(), json!(reminder.to_id));
        payload.insert("due_at".into(), json!(row.due_at));
        payload.insert("message_id".into(), json!(message.message_id));
        payload.extend(reminder.extra.clone());
        tx.emit("system", "request.reminded", payload.into())?;
        Ok(())
    });
    if let Err(e) = sent {
        tx.emit(
            "system",
            "request.notify_failed",
            json!({ "request_id": row.request_id, "kind": reminder.kind, "error": e.to_string() }),
        )?;
    }
    Ok(())
}

/// Tell the requester that its request went stale past its deadline. Nobody is told about
/// work an agent requested and then claimed itself.
fn send_overdue_notice_tx(tx: &Tx<'_>, row: &ReminderRow) -> Result<()> {
    if row.owner_id.as_deref() == Some(row.creator_id.as_str()) {
        return Ok(());
    }
    let rid = &row.request_id;
    let due = fmt_ts(row.due_at);
    let (from_id, body) = match &row.owner_id {
        Some(owner_id) => {
            let owner = crate::identity::get_agent(tx, owner_id)?;
            (
                owner_id.as_str(),
                format!(
                    "Passed its due time {due}. Owner: {}. Anyone may take it over with \
                     another request claim {rid}, or cancel it with another request cancel {rid}. \
                     {AUTOMATIC}",
                    owner.address
                ),
            )
        }
        None => (
            row.creator_id.as_str(),
            format!(
                "Passed its due time {due} with nobody claiming it. Anyone may take it with \
                 another request claim {rid}, or cancel it with another request cancel {rid}. \
                 {AUTOMATIC}"
            ),
        ),
    };
    send_reminder_tx(
        tx,
        row,
        Reminder {
            kind: "overdue",
            from_id,
            to_id: &row.creator_id,
            // The title may come from a subject stored before D75.
            subject: format!("Overdue: {}", crate::mail::single_line(&row.title, 290)),
            body,
            extra: serde_json::Map::new(),
        },
    )
}

/// Remind each owner whose claimed deadline is within its lead, once per owner and deadline.
/// Runs only in the full sweep (`sweep_tx`), not on every write.
/// A window missed entirely (nothing swept before the deadline) gets no late reminder: the
/// overdue notice covers it.
pub(crate) fn remind_due_soon_tx(tx: &Tx<'_>) -> Result<()> {
    if !reminders_enabled() {
        return Ok(());
    }
    let mut stmt = tx.prepare(
        "SELECT request_id, message_id, thread_id, title, created_by, owner_id, due_at, claimed_at
         FROM requests
         WHERE status = 'claimed' AND node_id = ?1 AND due_at > ?2 AND due_at <= ?3
           AND claimed_at IS NOT NULL AND owner_id IS NOT NULL AND owner_id != created_by",
    )?;
    let due = stmt
        .query_map(
            params![
                tx.node_id,
                tx.now,
                tx.now.saturating_add(REMINDER_LEAD_MAX_MS)
            ],
            |r| {
                Ok((
                    ReminderRow {
                        request_id: r.get(0)?,
                        message_id: r.get(1)?,
                        thread_id: r.get(2)?,
                        title: r.get(3)?,
                        creator_id: r.get(4)?,
                        owner_id: r.get(5)?,
                        due_at: r.get(6)?,
                    },
                    r.get::<_, i64>(7)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (row, claimed_at) in due {
        let Some(lead) = reminder_lead_ms(claimed_at, row.due_at) else {
            continue;
        };
        if row.due_at - lead > tx.now {
            continue;
        }
        let Some(owner_id) = row.owner_id.as_deref() else {
            continue;
        };
        let subject = format!("Due soon: {}", crate::mail::single_line(&row.title, 290));
        // The body starts with this deadline, so the reminder mail itself is the record.
        let marker = format!("Due at {} (", fmt_ts(row.due_at));
        // One reminder per (request, owner, deadline) for the life of the request: moving the
        // deadline away and back, or releasing and reclaiming, does not repeat it. A reminder
        // replies to its own request's message, which tells apart several requests in one
        // thread. The lookup reads only this thread (`messages_by_thread`) and the delivery's
        // key.
        let sent: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM messages m
               JOIN deliveries d ON d.message_id = m.message_id AND d.agent_id = ?3
               WHERE m.thread_id = ?1 AND m.reply_to = ?6 AND m.sender_id = ?2
                 AND m.subject = ?4 AND substr(m.body, 1, length(?5)) = ?5)",
            params![
                row.thread_id,
                row.creator_id,
                owner_id,
                subject,
                marker,
                row.message_id
            ],
            |r| r.get(0),
        )?;
        if sent {
            continue;
        }
        let rid = &row.request_id;
        let minutes = (row.due_at - tx.now + 59_999) / 60_000;
        let mut extra = serde_json::Map::new();
        extra.insert("claimed_at".into(), json!(claimed_at));
        send_reminder_tx(
            tx,
            &row,
            Reminder {
                kind: "due_soon",
                from_id: &row.creator_id,
                to_id: owner_id,
                subject,
                body: format!(
                    "{marker}in {minutes} min). Complete it with another request complete \
                     {rid}, or release it with another request release {rid} if you cannot \
                     finish. {AUTOMATIC}"
                ),
                extra,
            },
        )?;
    }
    Ok(())
}

/// Requests an agent should see in its inbox: open/stale ones addressed to it, and ones it owns.
pub(crate) fn list_requests_for_inbox(conn: &Connection, agent_id: &str) -> Result<Vec<Request>> {
    let mut stmt = conn.prepare(&format!(
        "{REQUEST_SELECT}
         WHERE (r.status IN ('open', 'stale') AND EXISTS (
                  SELECT 1 FROM deliveries d WHERE d.message_id = r.message_id AND d.agent_id = ?1))
            OR (r.owner_id = ?1 AND r.status IN ('claimed', 'stale'))
         ORDER BY r.created_at DESC"
    ))?;
    let rows = stmt.query_map([agent_id], request_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

impl Store {
    pub fn request(&self, request_id: &str) -> Result<Request> {
        self.read(|conn| get_request(conn, request_id))
    }

    pub fn requests(&self, filter: RequestFilter) -> Result<Vec<Request>> {
        self.read(|conn| {
            let owner = match &filter.owner {
                Some(o) => Some(resolve_agent(conn, self.node_id(), o)?.id),
                None => None,
            };
            let limit = if filter.limit == 0 { 100 } else { filter.limit };
            let mut stmt = conn.prepare(&format!(
                "{REQUEST_SELECT}
                 WHERE r.node_id = ?1 AND (?2 IS NULL OR r.status = ?2) AND (?3 IS NULL OR r.owner_id = ?3)
                 ORDER BY r.created_at DESC LIMIT ?4"
            ))?;
            let rows = stmt.query_map(
                params![
                    self.node_id(),
                    filter.status.map(|s| s.as_str()),
                    owner,
                    limit
                ],
                request_from_row,
            )?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Claim, release, complete or cancel a request.
    pub fn update_request(&self, input: RequestUpdate) -> Result<Request> {
        self.update_request_scoped(input, None)
    }

    /// [`Store::update_request`] for a remote caller: only a request the token's agent
    /// created, received or owns; any other looks missing, exactly as an unknown id does.
    pub fn update_request_scoped(
        &self,
        input: RequestUpdate,
        scope: Option<&crate::tokens::TokenScope>,
    ) -> Result<Request> {
        self.update_request_with_lease(input, scope, None, None)
    }

    pub fn update_request_with_lease(
        &self,
        input: RequestUpdate,
        scope: Option<&crate::tokens::TokenScope>,
        ttl_seconds: Option<i64>,
        expected: Option<&crate::LeaseRef>,
    ) -> Result<Request> {
        let ttl = crate::checkout_ttl(ttl_seconds)?;
        if let Some(note) = &input.note {
            crate::queue::within_bytes("request note", note, MAX_REQUEST_NOTE_BYTES)?;
        }
        self.write(|tx| {
            stale_overdue_tx(tx)?;
            let me = crate::identity::resolve_acting_agent(tx, tx.node_id, &input.agent)?;
            if input.action == RequestAction::Claim { crate::actor_leases::prepare_business_subject(tx, &me.id, crate::SubjectKind::Request, &input.request_id)?; }
            let unknown = || Error::NotFound(format!("unknown request {:?}", input.request_id));
            let (status, owner_id, creator_id, title, thread_id, message_id): (
                String,
                Option<String>,
                String,
                String,
                String,
                String,
            ) = tx
                .query_row(
                    "SELECT status, owner_id, created_by, title, thread_id, message_id
                     FROM requests WHERE request_id = ?1",
                    [&input.request_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
                )
                .optional()?
                .ok_or_else(unknown)?;
            if let Some(scope) = scope {
                let agent_id = &scope.agent_id;
                let recipient = tx
                    .query_row(
                        "SELECT 1 FROM deliveries WHERE message_id = ?1 AND agent_id = ?2",
                        params![message_id, agent_id],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if creator_id != *agent_id && owner_id.as_ref() != Some(agent_id) && !recipient
                {
                    return Err(unknown());
                }
            }
            let status: RequestStatus = status.parse()?;
            let owned_by_me = owner_id.as_deref() == Some(me.id.as_str());
            let owner_name = || -> Result<String> {
                match &owner_id {
                    Some(id) => Ok(crate::identity::get_agent(tx, id)?.address),
                    None => Ok("nobody".to_string()),
                }
            };
            let closed = matches!(status, RequestStatus::Done | RequestStatus::Cancelled);
            if closed {
                return Err(Error::Conflict(format!(
                    "request {} is already {}",
                    input.request_id,
                    status.as_str()
                )));
            }
            let rid = &input.request_id;
            match input.action {
                RequestAction::Claim => {
                    crate::actor_leases::projection_checkout(tx, &me.id, crate::SubjectKind::Request, rid, ttl, &format!("request:{rid}"))?;
                    if status == RequestStatus::Claimed && owned_by_me {
                        if input.due_in_seconds.is_some() {
                            let due_at = due_from(tx.now, input.due_in_seconds)?;
                            let previous: Option<i64> = tx.query_row(
                                "SELECT due_at FROM requests WHERE request_id = ?1",
                                [rid],
                                |r| r.get(0),
                            )?;
                            // A deadline move is a request transition like any other: the
                            // event log is the audit trail and the activity feed's source.
                            tx.emit(
                                &me.address,
                                "request.updated",
                                json!({ "request_id": rid, "due_at": due_at, "previous_due_at": previous }),
                            )?;
                            tx.execute(
                                "UPDATE requests SET due_at = ?2, updated_at = ?3 WHERE request_id = ?1",
                                params![rid, due_at, tx.now],
                            )?;
                        }
                    } else if status == RequestStatus::Claimed {
                        return Err(Error::Conflict(format!(
                            "request {rid} is claimed by {}; ask them to release it",
                            owner_name()?
                        )));
                    } else {
                        // Open, or stale (anyone may take over stale work). A past due
                        // time is cleared so the claim does not go stale immediately.
                        let due_at = match due_from(tx.now, input.due_in_seconds)? {
                            Some(d) => Some(d),
                            None => tx.query_row(
                                "SELECT due_at FROM requests WHERE request_id = ?1 AND due_at > ?2",
                                params![rid, tx.now],
                                |r| r.get::<_, Option<i64>>(0),
                            )
                            .optional()?
                            .flatten(),
                        };
                        tx.emit(
                            &me.address,
                            "request.claimed",
                            json!({ "request_id": rid, "owner_id": me.id, "previous_owner_id": owner_id, "due_at": due_at }),
                        )?;
                        tx.execute(
                            "UPDATE requests SET status = 'claimed', owner_id = ?2, due_at = ?3,
                               claimed_at = ?4, updated_at = ?4 WHERE request_id = ?1",
                            params![rid, me.id, due_at, tx.now],
                        )?;
                    }
                }
                RequestAction::Release => {
                    crate::actor_leases::validate_projection(tx, &me.id, &format!("request:{rid}"), expected)?;
                    if !owned_by_me {
                        return Err(Error::Forbidden(format!(
                            "request {rid} is owned by {}, not {}",
                            owner_name()?,
                            me.address
                        )));
                    }
                    tx.emit(&me.address, "request.released", json!({ "request_id": rid }))?;
                    crate::actor_leases::projection_release(tx, &format!("request:{rid}"), "released")?;
                    // A due time already past is cleared, as on takeover: otherwise the next
                    // sweep turns the released request stale again with no one acting.
                    tx.execute(
                        "UPDATE requests SET status = 'open', owner_id = NULL, claimed_at = NULL,
                           due_at = CASE WHEN due_at <= ?2 THEN NULL ELSE due_at END,
                           updated_at = ?2 WHERE request_id = ?1",
                        params![rid, tx.now],
                    )?;
                }
                RequestAction::Complete => {
                    crate::actor_leases::validate_projection(tx, &me.id, &format!("request:{rid}"), expected)?;
                    crate::actor_leases::projection_release(tx, &format!("request:{rid}"), "completed")?;
                    if owner_id.is_some() && !owned_by_me && status == RequestStatus::Claimed {
                        return Err(Error::Conflict(format!(
                            "request {rid} is claimed by {}; only the owner can complete it",
                            owner_name()?
                        )));
                    }
                    tx.emit(
                        &me.address,
                        "request.completed",
                        json!({ "request_id": rid, "owner_id": me.id, "note": input.note }),
                    )?;
                    tx.execute(
                        "UPDATE requests SET status = 'done', owner_id = ?2, note = ?3,
                           claimed_at = coalesce(claimed_at, ?4), closed_at = ?4, updated_at = ?4
                         WHERE request_id = ?1",
                        params![rid, me.id, input.note, tx.now],
                    )?;
                    if creator_id != me.id {
                        let creator = crate::identity::get_agent(tx, &creator_id)?;
                        let body = match &input.note {
                            Some(n) if !n.trim().is_empty() => n.clone(),
                            _ => format!("Request {rid} is done."),
                        };
                        send_tx(
                            tx,
                            &SendMessage {
                                from: me.address.clone(),
                                to: vec![creator.address],
                                // The title may come from a subject stored before D75.
                                subject: format!(
                                    "Done: {}",
                                    crate::mail::single_line(&title, 290)
                                ),
                                body,
                                importance: Importance::Normal,
                                reply_to: Some(message_id),
                                thread_id: Some(thread_id),
                                ..Default::default()
                            },
                        )?;
                    }
                }
                RequestAction::Cancel => {
                    crate::actor_leases::projection_release(tx, &format!("request:{rid}"), "cancelled")?;
                    if creator_id != me.id {
                        return Err(Error::Forbidden(format!(
                            "only the creator of request {rid} can cancel it"
                        )));
                    }
                    tx.emit(
                        &me.address,
                        "request.cancelled",
                        json!({ "request_id": rid, "note": input.note }),
                    )?;
                    tx.execute(
                        "UPDATE requests SET status = 'cancelled', note = ?2, closed_at = ?3,
                           updated_at = ?3 WHERE request_id = ?1",
                        params![rid, input.note, tx.now],
                    )?;
                }
            }
            touch(tx, &me.id)?;
            get_request(tx, rid)
        })
    }
}
