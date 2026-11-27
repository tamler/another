//! `another.agent/1`: proof-bearing actors, dependent session scopes, and one lease
//! implementation for temporary rights. Native bindings and display names are labels.

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::crypto::{request_nonce, sha256_hex};
use crate::store::{new_id, Store, Tx};
use crate::{checkout_ttl, Error, Result, DEFAULT_TTL_SECONDS};

/// `now` is the store clock of the calling transaction, never the process clock.
pub(crate) fn acting_agent(
    conn: &Connection,
    now: i64,
    node: &str,
    proof: &ActorProof,
) -> Result<crate::identity::AgentRef> {
    let actor = check_actor(conn, proof, now, true)?;
    Ok(crate::identity::AgentRef {
        id: actor.actor_id,
        address: actor.name,
        node_id: node.into(),
    })
}

pub(crate) fn prepare_business_subject(
    tx: &Tx<'_>,
    owner: &str,
    kind: SubjectKind,
    key: &str,
) -> Result<()> {
    if tx
        .query_row(
            "SELECT 1 FROM contract_actors WHERE actor_id = ?1",
            [owner],
            |_| Ok(()),
        )
        .optional()?
        .is_none()
    {
        return Ok(());
    }
    match kind {
        SubjectKind::Queue => crate::queue::discard_legacy_claims_tx(tx)?,
        SubjectKind::Resource => {
            let node: String = tx.query_row(
                "SELECT node_id FROM resources WHERE resource_id = ?1",
                [key],
                |r| r.get(0),
            )?;
            if node != tx.node_id {
                return Err(Error::Forbidden(
                    "NO_CALL_CONTEXT: foreign resource actor carriage is not implemented".into(),
                ));
            }
            tx.execute("UPDATE resource_leases SET released_at = ?2, release_reason = 'legacy_informational' WHERE resource_id = ?1 AND released_at IS NULL AND NOT EXISTS (SELECT 1 FROM contract_actors a WHERE a.actor_id = resource_leases.agent_id)", params![key, tx.now])?;
        }
        SubjectKind::Request => {
            tx.execute("UPDATE requests SET owner_id = NULL, claimed_at = NULL, status = 'open', updated_at = ?2 WHERE request_id = ?1 AND status = 'claimed' AND NOT EXISTS (SELECT 1 FROM contract_actors a WHERE a.actor_id = requests.owner_id)", params![key, tx.now])?;
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn validate_projection(
    tx: &Tx<'_>,
    owner: &str,
    projection: &str,
    expected: Option<&LeaseRef>,
) -> Result<()> {
    let id: Option<String> = tx
        .query_row(
            "SELECT lease_id FROM lease_projections WHERE projection_id = ?1",
            [projection],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = id {
        let lease = load_lease(tx, &id)?;
        if expected != Some(&LeaseRef::from(&lease)) {
            return Err(Error::Forbidden(
                "STALE_GENERATION: exact current lease reference is required".into(),
            ));
        }
        let actor = actor_view(tx, owner, tx.now)?;
        owned_live(tx, &actor, &LeaseRef::from(&lease))?;
    }
    Ok(())
}

/// A compatibility receipt still `pending` this long after it was written belongs to
/// an attempt that stopped before recording its outcome. Every step of a live call
/// finishes within its busy timeout, far inside this bound.
pub(crate) const PENDING_ORPHAN_MS: i64 = 60_000;

pub(crate) const INTERRUPTED_RECEIPT: &str = "INTERRUPTED: an earlier attempt with this request_id stopped before its outcome was recorded, and it is not re-run because its effect may have committed. Check the business record, then use a new request_id";

pub(crate) fn compatibility_call(
    store: &Store,
    proof: &ActorProof,
    request: &str,
    action: &impl Serialize,
    run: impl FnOnce(&Store, &str) -> Result<serde_json::Value>,
) -> Result<serde_json::Value> {
    let hash = fingerprint("compatibility", action)?;
    let pending = format!("pending:{hash}");
    let interrupted = format!("interrupted:{hash}");
    let cached = store.write(|tx| {
        let actor = check_actor(tx, proof, tx.now, false)?;
        let prior: Option<(String, String)> = tx.query_row("SELECT fingerprint, result FROM contract_receipts WHERE actor_id = ?1 AND session_epoch = ?2 AND request_id = ?3", params![actor.actor_id, actor.session_epoch, request], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        if let Some((fingerprint, result)) = prior {
            if fingerprint == pending {
                let since = serde_json::from_str::<serde_json::Value>(&result)
                    .ok()
                    .and_then(|v| v["pending_since"].as_i64())
                    .unwrap_or(i64::MIN);
                if tx.now.saturating_sub(since) < PENDING_ORPHAN_MS {
                    return Err(Error::Conflict("UNKNOWN_ACK: this request is pending; query its durable business record before retrying".into()));
                }
                // The attempt that wrote this receipt stopped (a crash or kill) before
                // recording its outcome. Settle it so it never reads as in flight; it is
                // not re-run, because its business effect may already have committed.
                tx.execute("UPDATE contract_receipts SET fingerprint = ?4 WHERE actor_id = ?1 AND session_epoch = ?2 AND request_id = ?3 AND fingerprint = ?5", params![actor.actor_id, actor.session_epoch, request, interrupted, pending])?;
                return Ok(Err(()));
            }
            if fingerprint == interrupted {
                return Ok(Err(()));
            }
            if fingerprint != hash { return Err(Error::Conflict("request_id reused with different input".into())); }
            return Ok(Ok(Some(serde_json::from_str(&result)?)));
        }
        check_actor(tx, proof, tx.now, true)?;
        tx.execute("INSERT INTO contract_receipts (actor_id, session_epoch, request_id, fingerprint, result) VALUES (?1, ?2, ?3, ?4, ?5)", params![actor.actor_id, actor.session_epoch, request, pending, json!({"pending_since": tx.now}).to_string()])?;
        Ok(Ok(None))
    })?;
    match cached {
        Ok(Some(result)) => return Ok(result),
        Ok(None) => {}
        // Reported outside the write so the settled receipt commits.
        Err(()) => return Err(Error::Conflict(INTERRUPTED_RECEIPT.into())),
    }
    let scoped = store.with_actor(proof);
    let result = store.actor_activity(proof).and_then(|reminder| {
        run(&scoped, &proof.actor_id).map(|mut value| {
            value["reminder"] = serde_json::json!(reminder);
            value
        })
    });
    store.write(|tx| {
        authenticate_actor(tx, proof, tx.now)?;
        match &result {
            Ok(result) => { tx.execute("UPDATE contract_receipts SET fingerprint = ?4, result = ?5 WHERE actor_id = ?1 AND session_epoch = ?2 AND request_id = ?3 AND fingerprint = ?6", params![proof.actor_id, proof.session_epoch, request, hash, serde_json::to_string(result)?, format!("pending:{hash}")])?; }
            Err(_) => { tx.execute("DELETE FROM contract_receipts WHERE actor_id = ?1 AND session_epoch = ?2 AND request_id = ?3 AND fingerprint = ?4", params![proof.actor_id, proof.session_epoch, request, format!("pending:{hash}")])?; }
        }
        Ok(())
    })?;
    result
}

pub(crate) fn projection_checkout(
    tx: &Tx<'_>,
    owner: &str,
    kind: SubjectKind,
    key: &str,
    ttl: i64,
    projection: &str,
) -> Result<()> {
    if tx
        .query_row(
            "SELECT 1 FROM contract_actors WHERE actor_id = ?1",
            [owner],
            |_| Ok(()),
        )
        .optional()?
        .is_none()
    {
        return Ok(());
    }
    let actor = actor_view(tx, owner, tx.now)?;
    if actor.closed_at.is_some() {
        return Err(Error::Forbidden("CLOSED_SESSION".into()));
    }
    let lease = checkout_tx(
        tx,
        &actor,
        &CheckoutRight {
            subject_kind: kind,
            subject_key: key.into(),
            ttl_seconds: Some(ttl),
        },
        "compatibility_checkout",
    )?;
    tx.execute("INSERT INTO lease_projections (projection_id, lease_id) VALUES (?1, ?2) ON CONFLICT(projection_id) DO UPDATE SET lease_id = excluded.lease_id", params![projection, lease.lease_id])?;
    Ok(())
}

pub(crate) fn projection_renew(tx: &Tx<'_>, projection: &str) -> Result<()> {
    let reference: Option<String> = tx
        .query_row(
            "SELECT lease_id FROM lease_projections WHERE projection_id = ?1",
            [projection],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = reference {
        let lease = load_lease(tx, &id)?;
        let actor = actor_view(tx, &lease.owner_actor_id, tx.now)?;
        if actor.closed_at.is_some() || actor.session_epoch != lease.session_epoch {
            return Err(Error::Forbidden("CLOSED_SESSION".into()));
        }
        renew_tx(
            tx,
            &actor,
            &LeaseRef::from(&lease),
            "explicit_subject_renew",
        )?;
    }
    Ok(())
}

pub(crate) fn projection_release(tx: &Tx<'_>, projection: &str, reason: &str) -> Result<()> {
    tx.execute("UPDATE right_leases SET released_at = ?2, release_reason = ?3 WHERE lease_id = (SELECT lease_id FROM lease_projections WHERE projection_id = ?1) AND released_at IS NULL", params![projection, tx.now, reason])?;
    Ok(())
}

/// Compatibility rows are projections of the common deadline, never another clock.
/// Reads of contract state remain pure; writes materialize expired responsibility.
/// Runs twice in every write transaction, so every statement starts from the open
/// business rows (unreleased holds and resource leases, claimed queue items and
/// requests, unlapsed resources) and reaches a projection by its key. Ended history is
/// never joined or rewritten again.
pub(crate) fn sync_projections(tx: &Tx<'_>) -> Result<()> {
    for (table, id) in [("holds", "hold_id"), ("resource_leases", "lease_id")] {
        tx.execute(&format!("UPDATE {table} SET ttl_seconds = (SELECT l.ttl_seconds FROM right_leases l JOIN lease_projections p ON p.lease_id = l.lease_id WHERE p.projection_id = {table}.{id}), renewed_at = (SELECT l.last_update_at FROM right_leases l JOIN lease_projections p ON p.lease_id = l.lease_id WHERE p.projection_id = {table}.{id}), expires_at = (SELECT l.expires_at FROM right_leases l JOIN lease_projections p ON p.lease_id = l.lease_id WHERE p.projection_id = {table}.{id}) WHERE released_at IS NULL AND EXISTS (SELECT 1 FROM lease_projections p WHERE p.projection_id = {table}.{id})"), [])?;
        tx.execute(&format!("UPDATE {table} SET released_at = ?1, release_reason = 'lease_ended' WHERE released_at IS NULL AND EXISTS (SELECT 1 FROM lease_projections p JOIN right_leases l ON l.lease_id = p.lease_id WHERE p.projection_id = {table}.{id} AND (l.released_at IS NOT NULL OR l.expires_at <= ?1))"), [tx.now])?;
    }
    let mut stmt = tx.prepare("SELECT p.projection_id FROM queue_items q JOIN lease_projections p ON p.projection_id = 'queue:' || q.item_id JOIN right_leases l ON l.lease_id = p.lease_id WHERE q.state = 'claimed' AND (l.released_at IS NOT NULL OR l.expires_at <= ?1)
        UNION ALL
        SELECT p.projection_id FROM requests r JOIN lease_projections p ON p.projection_id = 'request:' || r.request_id JOIN right_leases l ON l.lease_id = p.lease_id WHERE r.status = 'claimed' AND (l.released_at IS NOT NULL OR l.expires_at <= ?1)")?;
    let ended = stmt
        .query_map([tx.now], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ended {
        if let Some(item) = id.strip_prefix("queue:") {
            crate::queue::projection_ended_tx(tx, item)?;
        }
        if let Some(request) = id.strip_prefix("request:") {
            tx.execute("UPDATE requests SET owner_id = NULL, claimed_at = NULL, status = 'open', updated_at = ?2 WHERE request_id = ?1 AND status = 'claimed'", params![request, tx.now])?;
        }
    }
    crate::resources::settle_contract_projections(tx)?;
    Ok(())
}

pub const IDLE_REMINDER: &str =
    "You were inactive. Release what you no longer need; re-check expired leases before writing.";

/// A local integration grant. Keep it in the adapter's private store, never argv,
/// model context, normal environment, or logs.
#[derive(Clone, Serialize, Deserialize)]
pub struct ParticipantProof {
    pub participant_id: String,
    pub secret: String,
}

/// A private actor capability. Public actor ids and names cannot replace this proof.
#[derive(Clone, Serialize, Deserialize)]
pub struct ActorProof {
    pub actor_id: String,
    pub session_epoch: i64,
    pub secret: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActorView {
    pub actor_id: String,
    pub name: String,
    pub participant_id: String,
    pub native_key: String,
    pub parent_actor_id: Option<String>,
    pub session_scope_id: String,
    pub session_epoch: i64,
    pub closed_at: Option<i64>,
    pub last_actor_activity_at: i64,
    pub unused_stale: bool,
    pub reminder: Option<String>,
}

/// Registration replies contain a capability and must stay on the private channel.
#[derive(Clone, Serialize, Deserialize)]
pub struct RegisteredActor {
    pub actor: ActorView,
    pub proof: ActorProof,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubjectKind {
    File,
    Presence,
    Resource,
    Queue,
    Request,
    Wake,
    Delivery,
}

impl SubjectKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Presence => "presence",
            Self::Resource => "resource",
            Self::Queue => "queue",
            Self::Request => "request",
            Self::Wake => "wake",
            Self::Delivery => "delivery",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RightLease {
    pub lease_id: String,
    pub generation: i64,
    pub owner_actor_id: String,
    pub subject_kind: SubjectKind,
    pub subject_key: String,
    pub session_scope_id: String,
    pub session_epoch: i64,
    pub last_update_at: i64,
    pub ttl_seconds: i64,
    pub expires_at: i64,
    pub released_at: Option<i64>,
    pub evidence_source: String,
    pub release_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckoutRight {
    pub subject_kind: SubjectKind,
    pub subject_key: String,
    pub ttl_seconds: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeaseRef {
    pub lease_id: String,
    pub generation: i64,
}

impl From<&RightLease> for LeaseRef {
    fn from(lease: &RightLease) -> Self {
        Self {
            lease_id: lease.lease_id.clone(),
            generation: lease.generation,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EditOutcome {
    Success,
    FailedExecution,
    PermissionDenied,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EditReceipt {
    pub call_id: String,
    pub targets: Vec<LeaseRef>,
    pub outcome: Option<EditOutcome>,
    pub renewed: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationView {
    pub actor_id: String,
    pub session_epoch: i64,
    pub call_id: String,
    pub targets: Vec<LeaseRef>,
    pub admitted_at: i64,
    pub completed_at: Option<i64>,
    pub result: Option<EditReceipt>,
}

const ACTOR_SELECT: &str = "SELECT a.actor_id, a.name, a.participant_id, a.native_key,
    a.parent_actor_id, a.session_scope_id, s.epoch, s.closed_at,
    a.last_actor_activity_at, a.reminded_activity_at
    FROM contract_actors a JOIN session_scopes s ON s.scope_id = a.session_scope_id";

fn actor_view(conn: &Connection, id: &str, now: i64) -> Result<ActorView> {
    Ok(conn.query_row(
        &format!("{ACTOR_SELECT} WHERE a.actor_id = ?1"),
        [id],
        |r| {
            let activity: i64 = r.get(8)?;
            let reminded: Option<i64> = r.get(9)?;
            let stale = now.saturating_sub(activity) >= DEFAULT_TTL_SECONDS * 1000;
            Ok(ActorView {
                actor_id: r.get(0)?,
                name: r.get(1)?,
                participant_id: r.get(2)?,
                native_key: r.get(3)?,
                parent_actor_id: r.get(4)?,
                session_scope_id: r.get(5)?,
                session_epoch: r.get(6)?,
                closed_at: r.get(7)?,
                last_actor_activity_at: activity,
                unused_stale: stale,
                reminder: (stale && reminded != Some(activity)).then(|| IDLE_REMINDER.into()),
            })
        },
    )?)
}

fn no_actor() -> Error {
    Error::Forbidden("NO_ACTOR: a contract-minted private actor proof is required".into())
}

fn check_participant(conn: &Connection, proof: &ParticipantProof) -> Result<()> {
    let hash = sha256_hex(proof.secret.as_bytes());
    let found = conn
        .query_row(
            "SELECT 1 FROM contract_participants WHERE participant_id = ?1 AND proof_hash = ?2",
            params![proof.participant_id, hash],
            |_| Ok(()),
        )
        .optional()?;
    found.ok_or_else(no_actor)
}

fn authenticate_actor(conn: &Connection, proof: &ActorProof, now: i64) -> Result<ActorView> {
    let hash = sha256_hex(proof.secret.as_bytes());
    let found = conn.query_row(
        "SELECT 1 FROM contract_actors WHERE actor_id = ?1 AND proof_hash = ?2 AND retired_at IS NULL",
        params![proof.actor_id, hash],
        |_| Ok(()),
    ).optional()?;
    found.ok_or_else(no_actor)?;
    actor_view(conn, &proof.actor_id, now)
}

fn check_actor(conn: &Connection, proof: &ActorProof, now: i64, live: bool) -> Result<ActorView> {
    let actor = authenticate_actor(conn, proof, now)?;
    if actor.session_epoch != proof.session_epoch {
        return Err(Error::Forbidden(
            "STALE_EPOCH: resolve the current session epoch".into(),
        ));
    }
    if live && actor.closed_at.is_some() {
        return Err(Error::Forbidden(
            "CLOSED_SESSION: fresh checkout requires resume".into(),
        ));
    }
    Ok(actor)
}

fn valid_label(label: &str) -> Result<()> {
    if label.is_empty() || label.len() > 256 || label.chars().any(char::is_control) {
        return Err(Error::Invalid(
            "contract label must be 1-256 bytes without controls".into(),
        ));
    }
    Ok(())
}

fn valid_call_id(call_id: &str) -> Result<()> {
    // Cursor can put two native ids separated by a newline in tool_use_id. It is
    // opaque correlation, never a filesystem path or a printable notice.
    if call_id.is_empty() || call_id.len() > 4096 || call_id.contains('\0') {
        return Err(Error::Invalid(
            "call id must be 1-4096 bytes without NUL".into(),
        ));
    }
    Ok(())
}

const LEASE_SELECT: &str = "SELECT lease_id, generation, owner_actor_id, subject_kind,
    subject_key, session_scope_id, session_epoch, last_update_at, ttl_seconds,
    expires_at, released_at, evidence_source, release_reason FROM right_leases";

fn lease_row(r: &Row<'_>) -> rusqlite::Result<RightLease> {
    let raw: String = r.get(3)?;
    let kind = match raw.as_str() {
        "file" => SubjectKind::File,
        "presence" => SubjectKind::Presence,
        "resource" => SubjectKind::Resource,
        "queue" => SubjectKind::Queue,
        "request" => SubjectKind::Request,
        "wake" => SubjectKind::Wake,
        "delivery" => SubjectKind::Delivery,
        _ => return Err(rusqlite::Error::InvalidQuery),
    };
    Ok(RightLease {
        lease_id: r.get(0)?,
        generation: r.get(1)?,
        owner_actor_id: r.get(2)?,
        subject_kind: kind,
        subject_key: r.get(4)?,
        session_scope_id: r.get(5)?,
        session_epoch: r.get(6)?,
        last_update_at: r.get(7)?,
        ttl_seconds: r.get(8)?,
        expires_at: r.get(9)?,
        released_at: r.get(10)?,
        evidence_source: r.get(11)?,
        release_reason: r.get(12)?,
    })
}

fn load_lease(conn: &Connection, id: &str) -> Result<RightLease> {
    conn.query_row(
        &format!("{LEASE_SELECT} WHERE lease_id = ?1"),
        [id],
        lease_row,
    )
    .optional()?
    .ok_or_else(|| Error::NotFound("unknown lease".into()))
}

fn checkout_tx(
    tx: &Tx<'_>,
    actor: &ActorView,
    input: &CheckoutRight,
    source: &str,
) -> Result<RightLease> {
    let mut input = input.clone();
    if input.subject_key.is_empty()
        || input.subject_key.len() > 8192
        || input.subject_key.contains('\0')
    {
        return Err(Error::Invalid("invalid lease subject key".into()));
    }
    if input.subject_kind == SubjectKind::File {
        let path = std::path::Path::new(&input.subject_key);
        // The serving process's folder is not the caller's (a daemon or MCP server
        // serves many callers), so a relative target is refused, never guessed. The
        // caller makes it absolute from its own working folder.
        if !path.is_absolute() {
            return Err(Error::Invalid(
                "exact absolute file target required; resolve relative paths from the caller's working folder".into(),
            ));
        }
        let absolute = path.to_path_buf();
        let canonical =
            if std::fs::symlink_metadata(&absolute).is_ok_and(|m| m.file_type().is_symlink()) {
                crate::repo::resolve_existing(
                    absolute
                        .parent()
                        .ok_or_else(|| Error::Invalid("exact file target required".into()))?,
                )
                .join(
                    absolute
                        .file_name()
                        .ok_or_else(|| Error::Invalid("exact file target required".into()))?,
                )
            } else {
                crate::repo::resolve_existing(&absolute)
            };
        if canonical.is_dir() {
            return Err(Error::Invalid("exact file target required".into()));
        }
        let existing_parent = canonical.ancestors().skip(1).find(|p| p.is_dir());
        input.subject_key = if existing_parent.is_some_and(crate::holds::folds_case) {
            canonical.to_string_lossy().to_lowercase()
        } else {
            canonical.to_string_lossy().into_owned()
        };
    }
    if input.subject_key.is_empty()
        || input.subject_key.len() > 8192
        || input.subject_key.contains('\0')
    {
        return Err(Error::Invalid(
            "subject_key must be 1-8192 bytes without NUL".into(),
        ));
    }
    let ttl = checkout_ttl(input.ttl_seconds)?;
    let existing = tx
        .query_row(
            &format!(
                "{LEASE_SELECT} WHERE subject_kind = ?1 AND subject_key = ?2
            AND released_at IS NULL AND expires_at > ?3"
            ),
            params![input.subject_kind.as_str(), input.subject_key, tx.now],
            lease_row,
        )
        .optional()?;
    if let Some(lease) = existing {
        if lease.owner_actor_id == actor.actor_id && lease.session_epoch == actor.session_epoch {
            // Repeated admission is not evidence of a write and cannot move the deadline.
            return Ok(lease);
        }
        return Err(Error::Conflict(format!(
            "subject held by actor {} until {}",
            lease.owner_actor_id, lease.expires_at
        )));
    }
    let generation: i64 = tx.query_row(
        "SELECT coalesce(max(generation), 0) + 1 FROM right_leases WHERE subject_kind = ?1 AND subject_key = ?2",
        params![input.subject_kind.as_str(), input.subject_key], |r| r.get(0),
    )?;
    let lease = RightLease {
        lease_id: new_id(),
        generation,
        owner_actor_id: actor.actor_id.clone(),
        subject_kind: input.subject_kind,
        subject_key: input.subject_key.clone(),
        session_scope_id: actor.session_scope_id.clone(),
        session_epoch: actor.session_epoch,
        last_update_at: tx.now,
        ttl_seconds: ttl,
        expires_at: tx.now + ttl * 1000,
        released_at: None,
        evidence_source: source.into(),
        release_reason: None,
    };
    tx.execute(
        "INSERT INTO right_leases (lease_id, generation, owner_actor_id, subject_kind, subject_key,
          session_scope_id, session_epoch, last_update_at, ttl_seconds, expires_at, evidence_source)
          VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            lease.lease_id,
            generation,
            actor.actor_id,
            input.subject_kind.as_str(),
            input.subject_key,
            actor.session_scope_id,
            actor.session_epoch,
            tx.now,
            ttl,
            lease.expires_at,
            source
        ],
    )?;
    tx.emit(
        &actor.name,
        "lease.checked_out",
        serde_json::to_value(&lease).map_err(|e| Error::Invalid(e.to_string()))?,
    )?;
    Ok(lease)
}

fn owned_live(tx: &Tx<'_>, actor: &ActorView, target: &LeaseRef) -> Result<RightLease> {
    owned_live_at(tx, tx.now, actor, target)
}

fn owned_live_at(
    conn: &Connection,
    now: i64,
    actor: &ActorView,
    target: &LeaseRef,
) -> Result<RightLease> {
    let lease = load_lease(conn, &target.lease_id)?;
    if lease.owner_actor_id != actor.actor_id
        || lease.generation != target.generation
        || lease.session_epoch != actor.session_epoch
        || lease.session_scope_id != actor.session_scope_id
    {
        return Err(Error::Forbidden(
            "STALE_GENERATION: lease is not owned in this epoch".into(),
        ));
    }
    if lease.released_at.is_some() || now >= lease.expires_at {
        return Err(Error::Conflict(
            "EXPIRED_LEASE: a fresh checkout is required".into(),
        ));
    }
    Ok(lease)
}

fn renew_tx(tx: &Tx<'_>, actor: &ActorView, target: &LeaseRef, source: &str) -> Result<RightLease> {
    let mut lease = owned_live(tx, actor, target)?;
    lease.last_update_at = tx.now;
    lease.expires_at = tx.now + lease.ttl_seconds * 1000;
    lease.evidence_source = source.into();
    tx.execute("UPDATE right_leases SET last_update_at = ?2, expires_at = ?3, evidence_source = ?4 WHERE lease_id = ?1",
        params![lease.lease_id, tx.now, lease.expires_at, source])?;
    tx.emit(&actor.name, "lease.renewed", json!({"lease_id": lease.lease_id, "generation": lease.generation, "expires_at": lease.expires_at, "evidence_source": source}))?;
    Ok(lease)
}

fn fingerprint(operation: &str, input: &impl Serialize) -> Result<String> {
    Ok(sha256_hex(
        format!(
            "{operation}\n{}",
            serde_json::to_string(input).map_err(|e| Error::Invalid(e.to_string()))?
        )
        .as_bytes(),
    ))
}

fn replay<T: serde::de::DeserializeOwned>(
    tx: &Tx<'_>,
    actor: &ActorView,
    request: &str,
    hash: &str,
) -> Result<Option<T>> {
    valid_call_id(request)?;
    let saved: Option<(String, String)> = tx.query_row(
        "SELECT fingerprint, result FROM contract_receipts WHERE actor_id = ?1 AND session_epoch = ?2 AND request_id = ?3",
        params![actor.actor_id, actor.session_epoch, request], |r| Ok((r.get(0)?, r.get(1)?)),
    ).optional()?;
    saved
        .map(|(previous, result)| {
            if previous != hash {
                return Err(Error::Conflict(
                    "request_id reused with different input".into(),
                ));
            }
            serde_json::from_str(&result).map_err(|e| Error::Invalid(e.to_string()))
        })
        .transpose()
}

fn save(
    tx: &Tx<'_>,
    actor: &ActorView,
    request: &str,
    hash: &str,
    result: &impl Serialize,
) -> Result<()> {
    let result = serde_json::to_string(result).map_err(|e| Error::Invalid(e.to_string()))?;
    tx.execute("INSERT INTO contract_receipts (actor_id, session_epoch, request_id, fingerprint, result) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![actor.actor_id, actor.session_epoch, request, hash, result])?;
    Ok(())
}

impl Store {
    pub(crate) fn contract_action_activity(
        &self,
        proof: &ActorProof,
        request: &str,
        completion: bool,
    ) -> Result<Option<String>> {
        let skip = self.read(|conn| {
            let actor = authenticate_actor(conn, proof, self.now())?;
            if actor.closed_at.is_some() || proof.session_epoch != actor.session_epoch {
                return Ok(true);
            }
            let sql = if completion {
                "SELECT 1 FROM actor_operations WHERE actor_id = ?1 AND session_epoch = ?2 AND call_id = ?3 AND result IS NOT NULL"
            } else {
                "SELECT 1 FROM contract_receipts WHERE actor_id = ?1 AND session_epoch = ?2 AND request_id = ?3"
            };
            Ok(conn.query_row(sql, params![proof.actor_id, proof.session_epoch, request], |_| Ok(())).optional()?.is_some())
        })?;
        if skip {
            Ok(None)
        } else {
            self.actor_activity(proof)
        }
    }

    pub fn check_right(&self, proof: &ActorProof, target: &LeaseRef) -> Result<RightLease> {
        self.read(|conn| {
            let actor = check_actor(conn, proof, self.now(), true)?;
            owned_live_at(conn, self.now(), &actor, target)
        })
    }
    /// Validate an existing admission without renewing it. A saved begin receipt is
    /// an idempotency result, not permission to execute a completed or expired call.
    pub fn check_actor_edit(&self, proof: &ActorProof, call_id: &str) -> Result<Vec<RightLease>> {
        self.read(|conn| {
            let actor = check_actor(conn, proof, self.now(), true)?;
            let (targets, completed): (String, Option<i64>) = conn.query_row("SELECT targets, completed_at FROM actor_operations WHERE actor_id = ?1 AND session_epoch = ?2 AND call_id = ?3", params![actor.actor_id, actor.session_epoch, call_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?.ok_or_else(|| Error::Forbidden("NO_CALL_CONTEXT".into()))?;
            if completed.is_some() { return Err(Error::Forbidden("CLOSED_OPERATION: this call already completed".into())); }
            let targets: Vec<LeaseRef> = serde_json::from_str(&targets)?;
            targets.iter().map(|target| {
                let lease = load_lease(conn, &target.lease_id)?;
                if lease.owner_actor_id != actor.actor_id || lease.generation != target.generation || lease.session_epoch != actor.session_epoch || lease.released_at.is_some() || self.now() >= lease.expires_at {
                    return Err(Error::Forbidden("EXPIRED_LEASE: fresh admission is required".into()));
                }
                Ok(lease)
            }).collect()
        })
    }
    pub fn has_native_actor(
        &self,
        participant: &ParticipantProof,
        native_key: &str,
    ) -> Result<bool> {
        self.read(|conn| {
            check_participant(conn, participant)?;
            Ok(conn.query_row("SELECT 1 FROM contract_actors WHERE participant_id = ?1 AND native_key = ?2 AND retired_at IS NULL", params![participant.participant_id, native_key], |_| Ok(())).optional()?.is_some())
        })
    }

    /// Resolve under retained private participant authority, including closed epochs
    /// for late outcome history. This does not register, resume or renew anything.
    pub fn resolve_registered_actor(
        &self,
        participant: &ParticipantProof,
        native_key: &str,
    ) -> Result<RegisteredActor> {
        self.read(|conn| {
            check_participant(conn, participant)?;
            let id: String = conn.query_row("SELECT actor_id FROM contract_actors WHERE participant_id = ?1 AND native_key = ?2 AND retired_at IS NULL", params![participant.participant_id, native_key], |r| r.get(0)).optional()?.ok_or_else(no_actor)?;
            let actor = actor_view(conn, &id, self.now())?;
            let secret = sha256_hex(format!("another.actor/1\n{}\n{id}", participant.secret).as_bytes());
            Ok(RegisteredActor { proof: ActorProof { actor_id: id, session_epoch: actor.session_epoch, secret }, actor })
        })
    }
    /// Local same-user enrollment, called by an installed integration. The returned
    /// grant is private authority; a display name cannot retrieve or replace it.
    pub fn enroll_participant(&self) -> Result<ParticipantProof> {
        let proof = ParticipantProof {
            participant_id: new_id(),
            secret: request_nonce()?,
        };
        self.write(|tx| {
            tx.execute("INSERT INTO contract_participants (participant_id, proof_hash, created_at) VALUES (?1, ?2, ?3)",
                params![proof.participant_id, sha256_hex(proof.secret.as_bytes()), tx.now])?;
            tx.emit("operator", "contract.enrolled", json!({"participant_id": proof.participant_id}))?;
            Ok(())
        })?;
        Ok(proof)
    }

    /// Register a root or a dependent child before its first admission. A parent proof
    /// authorizes child registration, never edits on behalf of that child.
    pub fn register_actor(
        &self,
        participant: &ParticipantProof,
        native_key: &str,
        label: &str,
        parent: Option<&ActorProof>,
    ) -> Result<RegisteredActor> {
        valid_label(native_key)?;
        crate::identity::validate_name(label)?;
        self.write(|tx| {
            check_participant(tx, participant)?;
            let parent = parent.map(|p| check_actor(tx, p, tx.now, true)).transpose()?;
            if parent.as_ref().is_some_and(|p| p.participant_id != participant.participant_id) {
                return Err(no_actor());
            }
            let existing: Option<String> = tx.query_row(
                "SELECT actor_id FROM contract_actors WHERE participant_id = ?1 AND native_key = ?2",
                params![participant.participant_id, native_key], |r| r.get(0),
            ).optional()?;
            let actor_id = existing.clone().unwrap_or_else(new_id);
            let secret = sha256_hex(format!("another.actor/1\n{}\n{actor_id}", participant.secret).as_bytes());
            if existing.is_none() {
                let scope = new_id();
                let root = parent.as_ref().map_or_else(new_id, |p| p.name.split('@').next_back().unwrap_or(&p.name).to_owned());
                let name = match &parent {
                    Some(p) => format!("{label}.{}", p.name),
                    None => format!("{label}@{root}"),
                };
                tx.execute("INSERT INTO session_scopes (scope_id, parent_scope_id, parent_epoch, epoch, opened_at) VALUES (?1, ?2, ?3, 1, ?4)",
                    params![scope, parent.as_ref().map(|p| &p.session_scope_id), parent.as_ref().map(|p| p.session_epoch), tx.now])?;
                tx.execute("INSERT INTO contract_actors (actor_id, name, participant_id, native_key, parent_actor_id, session_scope_id, proof_hash, last_actor_activity_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![actor_id, name, participant.participant_id, native_key, parent.as_ref().map(|p| &p.actor_id), scope, sha256_hex(secret.as_bytes()), tx.now])?;
                let event = tx.emit(&name, "actor.registered", json!({"actor_id": actor_id, "session_scope_id": scope, "parent_actor_id": parent.as_ref().map(|p| &p.actor_id)}))?;
                // Compatibility projections use the minted id, never a public name
                // lookup that could adopt an earlier agent's claims.
                tx.execute("INSERT INTO agents (agent_id, node_id, name, address, program, status, created_at, updated_at, last_seen_at, event_id) VALUES (?1, ?2, ?1, ?3, 'other', 'active', ?4, ?4, ?4, ?5)", params![actor_id, tx.node_id, name, tx.now, event])?;
                // Public directory entry for peers, in the shape federation already
                // projects, so mail from this actor is delivered on other nodes. It
                // carries the public id only; grants and lease authority stay local.
                tx.emit(&name, "agent.registered", json!({"agent_id": actor_id, "name": actor_id, "project_id": null, "program": "other", "model": null, "description": null, "new": true}))?;
            }
            let actor = actor_view(tx, &actor_id, tx.now)?;
            if actor.parent_actor_id.as_deref() != parent.as_ref().map(|p| p.actor_id.as_str()) {
                return Err(Error::Conflict("native binding has a different parent".into()));
            }
            Ok(RegisteredActor { proof: ActorProof { actor_id, session_epoch: actor.session_epoch, secret }, actor })
        })
    }

    /// Resume a closed scope with retained private authority, opening a fresh epoch.
    /// Old callbacks cannot renew or release rights belonging to the resumed epoch.
    pub fn resume_actor(
        &self,
        proof: &ActorProof,
        parent: Option<&ActorProof>,
    ) -> Result<RegisteredActor> {
        self.write(|tx| {
            let actor = check_actor(tx, proof, tx.now, false)?;
            if actor.closed_at.is_none() {
                return Ok(RegisteredActor { actor, proof: proof.clone() });
            }
            let parent = parent.map(|p| check_actor(tx, p, tx.now, true)).transpose()?;
            if actor.parent_actor_id.as_deref() != parent.as_ref().map(|p| p.actor_id.as_str()) {
                return Err(no_actor());
            }
            tx.execute("UPDATE session_scopes SET epoch = epoch + 1, closed_at = NULL, opened_at = ?2, parent_epoch = ?3 WHERE scope_id = ?1",
                params![actor.session_scope_id, tx.now, parent.as_ref().map(|p| p.session_epoch)])?;
            tx.execute("UPDATE contract_actors SET last_actor_activity_at = ?2, reminded_activity_at = NULL WHERE actor_id = ?1", params![actor.actor_id, tx.now])?;
            let actor = actor_view(tx, &actor.actor_id, tx.now)?;
            tx.emit(&actor.name, "session.resumed", json!({"session_scope_id": actor.session_scope_id, "session_epoch": actor.session_epoch}))?;
            Ok(RegisteredActor { proof: ActorProof { session_epoch: actor.session_epoch, ..proof.clone() }, actor })
        })
    }

    pub fn checkout_right(
        &self,
        proof: &ActorProof,
        request: &str,
        input: CheckoutRight,
    ) -> Result<RightLease> {
        let hash = fingerprint("checkout", &input)?;
        self.write(|tx| {
            let actor = check_actor(tx, proof, tx.now, true)?;
            if let Some(saved) = replay(tx, &actor, request, &hash)? {
                return Ok(saved);
            }
            let lease = checkout_tx(tx, &actor, &input, "checkout")?;
            save(tx, &actor, request, &hash, &lease)?;
            Ok(lease)
        })
    }

    pub fn renew_right(
        &self,
        proof: &ActorProof,
        request: &str,
        target: LeaseRef,
    ) -> Result<RightLease> {
        let hash = fingerprint("renew", &target)?;
        self.write(|tx| {
            let actor = check_actor(tx, proof, tx.now, true)?;
            if let Some(saved) = replay(tx, &actor, request, &hash)? {
                return Ok(saved);
            }
            let lease = renew_tx(tx, &actor, &target, "explicit_subject_renew")?;
            save(tx, &actor, request, &hash, &lease)?;
            Ok(lease)
        })
    }

    pub fn release_right(
        &self,
        proof: &ActorProof,
        request: &str,
        target: LeaseRef,
    ) -> Result<RightLease> {
        let hash = fingerprint("release", &target)?;
        self.write(|tx| {
            let actor = check_actor(tx, proof, tx.now, true)?;
            if let Some(saved) = replay(tx, &actor, request, &hash)? { return Ok(saved); }
            let lease = owned_live(tx, &actor, &target)?;
            tx.execute("UPDATE right_leases SET released_at = ?2, release_reason = 'explicit' WHERE lease_id = ?1", params![lease.lease_id, tx.now])?;
            tx.emit(&actor.name, "lease.released", json!({"lease_id": lease.lease_id, "reason": "explicit"}))?;
            let lease = load_lease(tx, &lease.lease_id)?;
            save(tx, &actor, request, &hash, &lease)?;
            Ok(lease)
        })
    }

    /// Admission and operation references commit together. A retry reuses the original
    /// receipt even after expiry; it never authorizes another execution.
    pub fn begin_actor_edit(
        &self,
        proof: &ActorProof,
        call_id: &str,
        targets: &[String],
        ttl_seconds: Option<i64>,
    ) -> Result<EditReceipt> {
        valid_call_id(call_id)?;
        if targets.is_empty() || targets.len() > 1024 {
            return Err(Error::Invalid("an edit needs 1-1024 exact targets".into()));
        }
        self.write(|tx| {
            let actor = check_actor(tx, proof, tx.now, true)?;
            let hash = fingerprint("edit.begin", &(targets, ttl_seconds))?;
            if let Some(saved) = replay(tx, &actor, call_id, &hash)? { return Ok(saved); }
            let mut refs = Vec::new();
            for target in targets {
                let lease = checkout_tx(tx, &actor, &CheckoutRight { subject_kind: SubjectKind::File, subject_key: target.clone(), ttl_seconds }, "edit_admission")?;
                let reference = LeaseRef::from(&lease);
                if !refs.contains(&reference) { refs.push(reference); }
            }
            let receipt = EditReceipt { call_id: call_id.into(), targets: refs, outcome: None, renewed: Vec::new() };
            tx.execute("INSERT INTO actor_operations (actor_id, session_epoch, call_id, targets, admitted_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![actor.actor_id, actor.session_epoch, call_id, serde_json::to_string(&receipt.targets).map_err(|e| Error::Invalid(e.to_string()))?, tx.now])?;
            save(tx, &actor, call_id, &hash, &receipt)?;
            Ok(receipt)
        })
    }

    /// Late outcomes remain durable facts. Only an admitted execution's exact,
    /// still-live lease generations can be renewed.
    pub fn complete_actor_edit(
        &self,
        proof: &ActorProof,
        call_id: &str,
        outcome: EditOutcome,
    ) -> Result<EditReceipt> {
        self.write(|tx| {
            let mut actor = authenticate_actor(tx, proof, tx.now)?;
            if proof.session_epoch <= 0 || proof.session_epoch > actor.session_epoch {
                return Err(Error::Forbidden("STALE_EPOCH: unknown completion epoch".into()));
            }
            if proof.session_epoch != actor.session_epoch {
                // Retained proof can record an old outcome, never exercise rights
                // in the new epoch. The operation lookup below stays in the old one.
                actor.closed_at = Some(tx.now);
                actor.session_epoch = proof.session_epoch;
            }
            let saved: Option<(String, Option<String>)> = tx.query_row(
                "SELECT targets, result FROM actor_operations WHERE actor_id = ?1 AND session_epoch = ?2 AND call_id = ?3",
                params![actor.actor_id, actor.session_epoch, call_id], |r| Ok((r.get(0)?, r.get(1)?)),
            ).optional()?;
            let (targets, previous) = saved.ok_or_else(|| Error::Forbidden("NO_CALL_CONTEXT: completion has no admitted targets".into()))?;
            if let Some(result) = previous { return serde_json::from_str(&result).map_err(|e| Error::Invalid(e.to_string())); }
            let targets: Vec<LeaseRef> = serde_json::from_str(&targets).map_err(|e| Error::Invalid(e.to_string()))?;
            let mut receipt = EditReceipt { call_id: call_id.into(), targets, outcome: Some(outcome), renewed: Vec::new() };
            if actor.closed_at.is_none() && matches!(outcome, EditOutcome::Success | EditOutcome::FailedExecution) {
                for target in &receipt.targets {
                    if owned_live(tx, &actor, target).is_ok() {
                        let renewed = renew_tx(tx, &actor, target, "admitted_completion")?;
                        receipt.renewed.push(renewed.lease_id);
                    }
                }
            }
            tx.execute("UPDATE actor_operations SET completed_at = ?4, result = ?5 WHERE actor_id = ?1 AND session_epoch = ?2 AND call_id = ?3",
                params![actor.actor_id, actor.session_epoch, call_id, tx.now, serde_json::to_string(&receipt).map_err(|e| Error::Invalid(e.to_string()))?])?;
            tx.emit(&actor.name, "edit.completed", json!({"call_id": call_id, "session_epoch": actor.session_epoch, "outcome": outcome, "renewed": receipt.renewed}))?;
            Ok(receipt)
        })
    }

    /// Close exactly this epoch and its dependent scope tree. TTL and process-reap
    /// grace never postpone release. An old epoch cannot close a resumed session.
    pub fn end_actor_session(
        &self,
        proof: &ActorProof,
        native_event_id: &str,
    ) -> Result<Vec<String>> {
        valid_label(native_event_id)?;
        self.write(|tx| {
            let actor = check_actor(tx, proof, tx.now, false)?;
            let hash = fingerprint("session.end", &native_event_id)?;
            if let Some(saved) = replay(tx, &actor, native_event_id, &hash)? { return Ok(saved); }
            if actor.closed_at.is_some() {
                let ids: Vec<String> = Vec::new();
                save(tx, &actor, native_event_id, &hash, &ids)?;
                return Ok(ids);
            }
            let mut stmt = tx.prepare("WITH RECURSIVE tree(scope_id, epoch) AS (
                SELECT scope_id, epoch FROM session_scopes WHERE scope_id = ?1
                UNION ALL SELECT s.scope_id, s.epoch FROM session_scopes s JOIN tree t
                  ON s.parent_scope_id = t.scope_id AND s.parent_epoch = t.epoch
                ) SELECT scope_id, epoch FROM tree")?;
            let scopes = stmt.query_map([&actor.session_scope_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let mut ids = Vec::new();
            for (scope, epoch) in scopes {
                let mut stmt = tx.prepare("SELECT lease_id FROM right_leases WHERE session_scope_id = ?1 AND session_epoch = ?2 AND released_at IS NULL")?;
                ids.extend(stmt.query_map(params![scope, epoch], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?);
                tx.execute("UPDATE right_leases SET released_at = ?3, release_reason = 'session_end' WHERE session_scope_id = ?1 AND session_epoch = ?2 AND released_at IS NULL", params![scope, epoch, tx.now])?;
                let mut owners = tx.prepare("SELECT actor_id, name FROM contract_actors WHERE session_scope_id = ?1")?;
                let owners = owners.query_map([&scope], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
                for (owner, name) in owners {
                    release_compatibility_tx(tx, &owner, &name)?;
                }
                tx.execute("UPDATE session_scopes SET closed_at = ?3 WHERE scope_id = ?1 AND epoch = ?2", params![scope, epoch, tx.now])?;
            }
            for chunk in ids.chunks(10_000) {
                tx.emit(&actor.name, "lease.released", json!({"lease_ids": chunk, "reason": "session_end", "session_scope_id": actor.session_scope_id, "session_epoch": actor.session_epoch}))?;
            }
            tx.emit(&actor.name, "session.closed", json!({"session_scope_id": actor.session_scope_id, "session_epoch": actor.session_epoch, "native_event_id": native_event_id, "released_count": ids.len()}))?;
            save(tx, &actor, native_event_id, &hash, &ids)?;
            Ok(ids)
        })
    }

    /// Evaluate the prior idle episode before recording real attributed activity.
    /// Presence is its own subject and cannot renew any file, queue or resource lease.
    pub fn actor_activity(&self, proof: &ActorProof) -> Result<Option<String>> {
        self.write(|tx| {
            let actor = check_actor(tx, proof, tx.now, true)?;
            let reminder = actor.reminder.clone();
            tx.execute("UPDATE contract_actors SET last_actor_activity_at = ?2, reminded_activity_at = CASE WHEN ?3 THEN ?4 ELSE reminded_activity_at END WHERE actor_id = ?1",
                params![actor.actor_id, tx.now, reminder.is_some(), actor.last_actor_activity_at])?;
            let presence = checkout_tx(tx, &actor, &CheckoutRight { subject_kind: SubjectKind::Presence, subject_key: actor.actor_id.clone(), ttl_seconds: None }, "attributed_activity")?;
            if presence.last_update_at != tx.now {
                renew_tx(tx, &actor, &LeaseRef::from(&presence), "attributed_activity")?;
            }
            Ok(reminder)
        })
    }

    /// Pure projection: status neither acknowledges reminders nor touches leases.
    pub fn contract_actors(&self) -> Result<Vec<ActorView>> {
        let now = self.now();
        self.read(|conn| {
            let mut stmt = conn.prepare("SELECT actor_id FROM contract_actors ORDER BY name")?;
            let ids = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            ids.iter().map(|id| actor_view(conn, id, now)).collect()
        })
    }

    /// Includes released and expired history; effectiveness is computed at authority
    /// time, independently of maintenance, transport observations, or physical cleanup.
    /// One actor's live leases, through the session-scope index rather than every lease.
    pub(crate) fn live_leases_of(&self, actor_id: &str) -> Result<Vec<RightLease>> {
        let now = self.now();
        self.read(|conn| {
            let mut stmt = conn.prepare(&format!(
                "{LEASE_SELECT} WHERE session_scope_id = (SELECT session_scope_id FROM contract_actors WHERE actor_id = ?1)
                   AND owner_actor_id = ?1 AND released_at IS NULL AND expires_at > ?2
                 ORDER BY last_update_at, lease_id"
            ))?;
            let leases = stmt
                .query_map(params![actor_id, now], lease_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(leases)
        })
    }

    pub fn right_leases(&self) -> Result<Vec<RightLease>> {
        self.read(|conn| {
            let mut stmt =
                conn.prepare(&format!("{LEASE_SELECT} ORDER BY last_update_at, lease_id"))?;
            let leases = stmt
                .query_map([], lease_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(leases)
        })
    }

    /// Durable exact-target evidence. Reading never completes or renews a call.
    pub fn actor_operations(&self) -> Result<Vec<OperationView>> {
        self.read(|conn| {
            let mut stmt = conn.prepare("SELECT actor_id, session_epoch, call_id, targets, admitted_at, completed_at, result FROM actor_operations ORDER BY admitted_at, actor_id, call_id")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, i64>(4)?, r.get::<_, Option<i64>>(5)?, r.get::<_, Option<String>>(6)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().map(|(actor_id, session_epoch, call_id, targets, admitted_at, completed_at, result)| Ok(OperationView {
                actor_id, session_epoch, call_id,
                targets: serde_json::from_str(&targets).map_err(|_| Error::Invalid("invalid operation targets".into()))?,
                admitted_at, completed_at,
                result: result.map(|raw| serde_json::from_str(&raw).map_err(|_| Error::Invalid("invalid operation result".into()))).transpose()?,
            })).collect()
        })
    }
}

fn release_compatibility_tx(tx: &Tx<'_>, owner: &str, name: &str) -> Result<()> {
    let mut stmt =
        tx.prepare("SELECT hold_id FROM holds WHERE agent_id = ?1 AND released_at IS NULL")?;
    let holds = stmt
        .query_map([owner], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    crate::holds::release_tx(tx, name, &holds, "session_end")?;
    let agent = crate::identity::AgentRef {
        id: owner.into(),
        address: name.into(),
        node_id: tx.node_id.into(),
    };
    crate::resources::session_leases_tx(tx, &agent, true, None)?;
    crate::queue::actor_ended_tx(tx, owner)?;
    tx.execute("UPDATE requests SET owner_id = NULL, status = 'open', claimed_at = NULL, updated_at = ?2 WHERE owner_id = ?1 AND status = 'claimed'", params![owner, tx.now])?;
    tx.execute(
        "UPDATE agents SET status = 'offline' WHERE agent_id = ?1",
        [owner],
    )?;
    tx.execute(
        "DELETE FROM transport_sessions WHERE agent_id = ?1",
        [owner],
    )?;
    tx.execute(
        "DELETE FROM wake_targets WHERE agent_id = ?1 AND auto = 1",
        [owner],
    )?;
    Ok(())
}

#[cfg(test)]
mod receipt_tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::Clock;

    /// A process killed between the pending receipt and its outcome leaves a
    /// `pending:` row. A retry inside the bound still reads as in flight; after the bound
    /// the receipt is settled as interrupted and never re-run, and a new request id works.
    #[test]
    fn a_crash_orphaned_pending_receipt_is_settled_not_left_in_flight() {
        let (clock, now) = Clock::manual(1_800_000_000_000);
        let store = Store::open_in_memory(clock).unwrap();
        let participant = store.enroll_participant().unwrap();
        let actor = store
            .register_actor(&participant, "root", "codex", None)
            .unwrap();
        let action = json!({"action": "probe"});
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            compatibility_call(&store, &actor.proof, "req", &action, |_, _| {
                panic!("process killed mid-call")
            })
        }));
        assert!(crashed.is_err());
        let runs = std::cell::Cell::new(0);
        let retry = |id: &str| {
            compatibility_call(&store, &actor.proof, id, &action, |_, _| {
                runs.set(runs.get() + 1);
                Ok(json!({"ran": true}))
            })
        };
        match retry("req") {
            Err(Error::Conflict(m)) => assert!(m.starts_with("UNKNOWN_ACK"), "{m}"),
            other => panic!("expected UNKNOWN_ACK inside the bound: {:?}", other.ok()),
        }
        now.fetch_add(PENDING_ORPHAN_MS, Ordering::SeqCst);
        for _ in 0..2 {
            match retry("req") {
                Err(Error::Conflict(m)) => assert!(m.starts_with("INTERRUPTED"), "{m}"),
                other => panic!("expected INTERRUPTED after the bound: {:?}", other.ok()),
            }
        }
        assert_eq!(runs.get(), 0, "an interrupted attempt is never re-run");
        let settled: String = store
            .read(|c| {
                Ok(c.query_row(
                    "SELECT fingerprint FROM contract_receipts WHERE request_id = 'req'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert!(settled.starts_with("interrupted:"), "{settled}");
        assert_eq!(retry("fresh").unwrap()["ran"], true);
        assert_eq!(runs.get(), 1);
    }
}
