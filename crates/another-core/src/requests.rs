//! Requests: owned work items opened from a message.
//!
//! Lifecycle: `open → claimed → done`, with `stale` when the due time passes and
//! `cancelled` when the creator withdraws it. A stale request may be claimed by anyone.

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{Error, Result};
use crate::groups::{is_member, resolve_group_for, GroupRef, Priority};
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
    pub priority: Priority,
    /// The group whose queue holds this request (`name@project`), if any.
    pub group: Option<String>,
}

/// Input for [`Store::request_next`].
#[derive(Debug, Clone, Default)]
pub struct NextRequest {
    pub agent: String,
    /// Only this group (`name` or `name@project`); otherwise every group the agent is in.
    pub group: Option<String>,
    /// Also take over stale items (after open ones of the same priority).
    pub include_stale: bool,
    /// Deadline for the claimed item, seconds from now.
    pub due_in_seconds: Option<i64>,
}

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
    /// Only items of this group's queue (`name@project`, or `name` in the owner's project).
    pub group: Option<String>,
    pub limit: u32,
}

const REQUEST_SELECT: &str = "SELECT r.request_id, r.title, r.status, o.address, NULL, c.address, NULL,
       r.thread_id, r.message_id, r.due_at, r.note, r.created_at, r.updated_at, r.claimed_at, r.closed_at,
       r.priority, g.name, gp.slug
     FROM requests r
     JOIN agents c ON c.agent_id = r.created_by
     LEFT JOIN projects cp ON cp.project_id = c.project_id
     LEFT JOIN agents o ON o.agent_id = r.owner_id
     LEFT JOIN projects op ON op.project_id = o.project_id
     LEFT JOIN groups g ON g.group_id = r.group_id
     LEFT JOIN projects gp ON gp.project_id = g.project_id";

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
        priority: Priority::from_rank(r.get(15)?),
        group: match (
            r.get::<_, Option<String>>(16)?,
            r.get::<_, Option<String>>(17)?,
        ) {
            (Some(name), Some(slug)) => Some(format!("{name}@{slug}")),
            _ => None,
        },
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

/// The queue fields of a request that a group queues.
struct QueueRow {
    group_id: String,
    priority: Priority,
    claimed_at: Option<i64>,
    due_at: Option<i64>,
}

fn due_from(now: i64, secs: Option<i64>) -> Result<Option<i64>> {
    match secs {
        None => Ok(None),
        Some(s) if s > 0 => later_secs(now, s, "due time").map(Some),
        Some(_) => Err(Error::Invalid("due_in_seconds must be positive".into())),
    }
}

/// Open a request for a message, with its priority and, for a queue item, its group.
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_queue_request_tx(
    tx: &Tx<'_>,
    request_id: &str,
    creator: &AgentRef,
    message_id: &str,
    thread_id: &str,
    title: &str,
    due_in_seconds: Option<i64>,
    group: Option<&GroupRef>,
    priority: Priority,
) -> Result<()> {
    let due_at = due_from(tx.now, due_in_seconds)?;
    let mut payload = json!({
        "request_id": request_id,
        "message_id": message_id,
        "thread_id": thread_id,
        "title": title,
        "due_at": due_at,
        "priority": priority.as_str(),
    });
    if let Some(g) = group {
        payload["group_id"] = json!(g.id);
    }
    tx.emit(&creator.address, "request.created", payload)?;
    tx.execute(
        "INSERT INTO requests (request_id, node_id, message_id, thread_id, title, created_by,
           status, due_at, created_at, updated_at, group_id, priority)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'open', ?7, ?8, ?8, ?9, ?10)",
        params![
            request_id,
            tx.node_id,
            message_id,
            thread_id,
            title,
            creator.id,
            due_at,
            tx.now,
            group.map(|g| g.id.as_str()),
            priority.rank()
        ],
    )?;
    Ok(())
}

/// Give the claimer of a queue item a delivery of its message, idempotently: a new
/// delivery is inserted read and announced (so no hook or wake presents it as new mail);
/// an existing one (a direct recipient, or an earlier claim of the same item) is left exactly
/// as it is. Claiming work is not reading mail.
fn ensure_claimer_delivery(tx: &Tx<'_>, message_id: &str, agent_id: &str) -> Result<()> {
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO deliveries (message_id, agent_id, node_id, role, read_at)
         VALUES (?1, ?2, ?3, 'queue', ?4)",
        params![message_id, agent_id, tx.node_id, tx.now],
    )?;
    if inserted == 1 {
        tx.execute(
            "INSERT OR IGNORE INTO announcements (agent_id, message_id, channel, announced_at)
             VALUES (?1, ?2, 'queue', ?3)",
            params![agent_id, message_id, tx.now],
        )?;
    }
    Ok(())
}

/// Mark open/claimed requests past their due time as stale. Returns how many changed.
pub(crate) fn stale_overdue_tx(tx: &Tx<'_>) -> Result<usize> {
    let mut stmt = tx.prepare(
        "SELECT request_id, status, group_id FROM requests
         WHERE node_id = ?1 AND status IN ('open', 'claimed') AND due_at IS NOT NULL AND due_at <= ?2",
    )?;
    let overdue = stmt
        .query_map(params![tx.node_id, tx.now], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, from, group_id) in &overdue {
        let mut payload = json!({ "request_id": id, "from": from, "reason": "past due" });
        if let Some(g) = group_id {
            payload["group_id"] = json!(g);
        }
        tx.emit("system", "request.stale", payload)?;
        tx.execute(
            "UPDATE requests SET status = 'stale', updated_at = ?2 WHERE request_id = ?1",
            params![id, tx.now],
        )?;
    }
    Ok(overdue.len())
}

/// Requests an agent should see in its inbox: open/stale ones addressed to it, and ones it owns.
pub(crate) fn list_requests_for_inbox(conn: &Connection, agent_id: &str) -> Result<Vec<Request>> {
    let mut stmt = conn.prepare(&format!(
        "{REQUEST_SELECT}
         WHERE (r.status IN ('open', 'stale') AND EXISTS (
                  SELECT 1 FROM deliveries d WHERE d.message_id = r.message_id AND d.agent_id = ?1
                    AND d.role != 'queue'))
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
        self.sweep()?;
        self.read(|conn| {
            let owner = match &filter.owner {
                Some(o) => Some(resolve_agent(conn, self.node_id(), o)?.id),
                None => None,
            };
            let group = match &filter.group {
                Some(g) => Some(crate::groups::resolve_group(conn, self.node_id(), g, None)?.id),
                None => None,
            };
            let limit = if filter.limit == 0 { 100 } else { filter.limit };
            let mut stmt = conn.prepare(&format!(
                "{REQUEST_SELECT}
                 WHERE r.node_id = ?1 AND (?2 IS NULL OR r.status = ?2) AND (?3 IS NULL OR r.owner_id = ?3)
                   AND (?5 IS NULL OR r.group_id = ?5)
                 ORDER BY r.created_at DESC LIMIT ?4"
            ))?;
            let rows = stmt.query_map(
                params![
                    self.node_id(),
                    filter.status.map(|s| s.as_str()),
                    owner,
                    limit,
                    group
                ],
                request_from_row,
            )?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Atomically take the next item from the agent's group queues: the first eligible item
    /// by priority, then (with `include_stale`) open before stale, then due time (none last),
    /// then age, then id. Selection and claim run in one write transaction, so two agents can
    /// never claim the same item from the same state. `None` when nothing is eligible; then
    /// nothing of its own is written (the shared overdue sweep still records real `stale`
    /// transitions, as every request write does).
    pub fn request_next(&self, input: NextRequest) -> Result<Option<Request>> {
        self.write(|tx| {
            stale_overdue_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, &input.agent)?;
            let groups: Vec<String> = match &input.group {
                Some(spec) => {
                    let g = resolve_group_for(tx, tx.node_id, &me, spec)?;
                    if !is_member(tx, &g.id, &me.id)? {
                        return Err(Error::Forbidden(format!(
                            "{} is not a member of {}",
                            me.address,
                            g.address()
                        )));
                    }
                    vec![g.id]
                }
                None => {
                    let mut stmt = tx.prepare(
                        "SELECT m.group_id FROM group_members m JOIN groups g ON g.group_id = m.group_id
                         WHERE m.agent_id = ?1 AND g.deleted_at IS NULL",
                    )?;
                    let ids = stmt
                        .query_map([&me.id], |r| r.get::<_, String>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    ids
                }
            };
            if groups.is_empty() {
                return Ok(None);
            }
            let groups_json =
                serde_json::to_string(&groups).map_err(|e| Error::Invalid(e.to_string()))?;
            let statuses = if input.include_stale {
                "('open', 'stale')"
            } else {
                "('open')"
            };
            let picked: Option<(String, String, Option<String>, String, i64, String)> = tx
                .query_row(
                    &format!(
                        "SELECT request_id, status, owner_id, group_id, priority, message_id FROM requests
                         WHERE node_id = ?1 AND group_id IN (SELECT value FROM json_each(?2))
                           AND status IN {statuses}
                         ORDER BY priority, CASE status WHEN 'open' THEN 0 ELSE 1 END,
                           due_at IS NULL, due_at, created_at, rowid, request_id
                         LIMIT 1"
                    ),
                    params![tx.node_id, groups_json],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
                )
                .optional()?;
            let Some((rid, status, previous, group_id, priority, message_id)) = picked else {
                return Ok(None);
            };
            let due_at = match due_from(tx.now, input.due_in_seconds)? {
                Some(d) => Some(d),
                None => tx
                    .query_row(
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
                json!({
                    "request_id": rid, "owner_id": me.id, "previous_owner_id": previous,
                    "due_at": due_at, "group_id": group_id,
                    "priority": Priority::from_rank(priority).as_str(), "via": "next",
                }),
            )?;
            let changed = tx.execute(
                "UPDATE requests SET status = 'claimed', owner_id = ?2, due_at = ?3,
                   claimed_at = ?4, updated_at = ?4 WHERE request_id = ?1 AND status = ?5",
                params![rid, me.id, due_at, tx.now, status],
            )?;
            if changed != 1 {
                return Err(Error::Conflict(format!(
                    "request {rid} changed while it was being claimed"
                )));
            }
            ensure_claimer_delivery(tx, &message_id, &me.id)?;
            touch(tx, &me.id)?;
            get_request(tx, &rid).map(Some)
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
        self.write(|tx| {
            stale_overdue_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, &input.agent)?;
            let unknown = || Error::NotFound(format!("unknown request {:?}", input.request_id));
            #[allow(clippy::type_complexity)]
            let (status, owner_id, creator_id, title, thread_id, message_id, queue): (
                String,
                Option<String>,
                String,
                String,
                String,
                String,
                Option<QueueRow>,
            ) = tx
                .query_row(
                    "SELECT status, owner_id, created_by, title, thread_id, message_id,
                       group_id, priority, claimed_at, due_at
                     FROM requests WHERE request_id = ?1",
                    [&input.request_id],
                    |r| {
                        let group_id: Option<String> = r.get(6)?;
                        let queue = match group_id {
                            Some(group_id) => Some(QueueRow {
                                group_id,
                                priority: Priority::from_rank(r.get(7)?),
                                claimed_at: r.get(8)?,
                                due_at: r.get(9)?,
                            }),
                            None => None,
                        };
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, queue))
                    },
                )
                .optional()?
                .ok_or_else(unknown)?;
            if scope.is_some() && queue.is_some() {
                return Err(Error::Forbidden(
                    "group queues are local-only in this release; act on this item from this machine"
                        .into(),
                ));
            }
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
            // Fields every event about a queue item carries.
            let with_queue = |mut payload: serde_json::Value| {
                if let Some(q) = &queue {
                    payload["group_id"] = json!(q.group_id);
                    payload["priority"] = json!(q.priority.as_str());
                }
                payload
            };
            // Taking a queue item, by claim or by completing it unclaimed, is for members. The
            // owner may still complete its own item after leaving, and the sender its own.
            let takes = match input.action {
                RequestAction::Claim => true,
                RequestAction::Complete => !owned_by_me && creator_id != me.id,
                RequestAction::Release | RequestAction::Cancel => false,
            };
            if let (true, Some(q)) = (takes, &queue) {
                if !is_member(tx, &q.group_id, &me.id)? {
                    return Err(Error::Forbidden(format!(
                        "{} is not a member of the group that queues request {rid}; join it first",
                        me.address
                    )));
                }
            }
            match input.action {
                RequestAction::Claim => {
                    if status == RequestStatus::Claimed && owned_by_me {
                        if input.due_in_seconds.is_some() {
                            let due_at = due_from(tx.now, input.due_in_seconds)?;
                            if let Some(q) = &queue {
                                tx.emit(
                                    &me.address,
                                    "request.updated",
                                    with_queue(json!({
                                        "request_id": rid,
                                        "due_at": { "from": q.due_at, "to": due_at },
                                    })),
                                )?;
                            }
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
                        let mut payload = json!({ "request_id": rid, "owner_id": me.id, "previous_owner_id": owner_id, "due_at": due_at });
                        if queue.is_some() {
                            payload["via"] = json!("id");
                        }
                        tx.emit(&me.address, "request.claimed", with_queue(payload))?;
                        tx.execute(
                            "UPDATE requests SET status = 'claimed', owner_id = ?2, due_at = ?3,
                               claimed_at = ?4, updated_at = ?4 WHERE request_id = ?1",
                            params![rid, me.id, due_at, tx.now],
                        )?;
                        if queue.is_some() {
                            ensure_claimer_delivery(tx, &message_id, &me.id)?;
                        }
                    }
                }
                RequestAction::Release => {
                    if !owned_by_me {
                        return Err(Error::Forbidden(format!(
                            "request {rid} is owned by {}, not {}",
                            owner_name()?,
                            me.address
                        )));
                    }
                    tx.emit(
                        &me.address,
                        "request.released",
                        with_queue(json!({ "request_id": rid })),
                    )?;
                    tx.execute(
                        "UPDATE requests SET status = 'open', owner_id = NULL, claimed_at = NULL,
                           updated_at = ?2 WHERE request_id = ?1",
                        params![rid, tx.now],
                    )?;
                    if queue.is_some() {
                        // A released queue item goes back to the queue as open work; a past
                        // deadline would turn it stale again at once.
                        tx.execute(
                            "UPDATE requests SET due_at = NULL WHERE request_id = ?1 AND due_at <= ?2",
                            params![rid, tx.now],
                        )?;
                    }
                }
                RequestAction::Complete => {
                    if owner_id.is_some() && !owned_by_me && status == RequestStatus::Claimed {
                        return Err(Error::Conflict(format!(
                            "request {rid} is claimed by {}; only the owner can complete it",
                            owner_name()?
                        )));
                    }
                    let mut payload = json!({ "request_id": rid, "owner_id": me.id, "note": input.note });
                    if let Some(q) = &queue {
                        // Throughput: claim to completion. An item completed without a claim
                        // has no claim time, so no work time (not zero).
                        let claimed = q.claimed_at.filter(|_| status != RequestStatus::Open);
                        payload["claimed_at"] = json!(claimed.map(fmt_ts));
                        payload["duration_ms"] = json!(claimed.map(|c| tx.now - c));
                    }
                    tx.emit(&me.address, "request.completed", with_queue(payload))?;
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
                                subject: format!("Done: {title}"),
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
                    if creator_id != me.id {
                        return Err(Error::Forbidden(format!(
                            "only the creator of request {rid} can cancel it"
                        )));
                    }
                    tx.emit(
                        &me.address,
                        "request.cancelled",
                        with_queue(json!({ "request_id": rid, "note": input.note })),
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
