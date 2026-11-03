//! Outbound webhooks (docs/design/02-push.md §3): subscriptions with project globs and
//! named filters, a delivery queue fed from the event log, HMAC-SHA256 signatures, retries
//! with backoff and a dead-letter list.

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::activity::{item, EventRow};
use crate::error::{Error, Result};
use crate::store::{new_id, Store};
use crate::time::fmt_ts;
use crate::tokens::{random_hex, TokenScope};
use crate::visibility::event_visible;

/// Named groups of event kinds a webhook can subscribe to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Hash)]
#[serde(rename_all = "snake_case")]
pub enum WebhookFilter {
    /// `lane.created`, `lane.status_changed`, `lane.closed`.
    LaneStatus,
    /// `lane.reported`.
    LaneReport,
    /// A lane moving to `needs_decision`; `decision.created`, `decision.overdue`.
    NeedsDecision,
    /// `request.created`, `decision.created`.
    RequestCreated,
    /// `request.completed`, `request.cancelled`, `decision.answered`, `decision.expired`.
    RequestAnswered,
    /// `hold.conflict` (an acquire was refused) and `lane.hold_conflict` (between lanes).
    HoldConflict,
    /// `resource.lease_expiring`: a lease has under five minutes left.
    LeaseExpiring,
    /// Everything visible except mail and delivery events.
    All,
}

impl WebhookFilter {
    pub const ALL: [WebhookFilter; 8] = [
        WebhookFilter::LaneStatus,
        WebhookFilter::LaneReport,
        WebhookFilter::NeedsDecision,
        WebhookFilter::RequestCreated,
        WebhookFilter::RequestAnswered,
        WebhookFilter::HoldConflict,
        WebhookFilter::LeaseExpiring,
        WebhookFilter::All,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            WebhookFilter::LaneStatus => "lane_status",
            WebhookFilter::LaneReport => "lane_report",
            WebhookFilter::NeedsDecision => "needs_decision",
            WebhookFilter::RequestCreated => "request_created",
            WebhookFilter::RequestAnswered => "request_answered",
            WebhookFilter::HoldConflict => "hold_conflict",
            WebhookFilter::LeaseExpiring => "lease_expiring",
            WebhookFilter::All => "all",
        }
    }

    /// Whether an event matches this filter.
    pub fn matches(self, kind: &str, payload: &Value) -> bool {
        let field = |k: &str| payload.get(k).and_then(Value::as_str);
        match self {
            WebhookFilter::LaneStatus => {
                matches!(kind, "lane.created" | "lane.status_changed" | "lane.closed")
            }
            WebhookFilter::LaneReport => kind == "lane.reported",
            WebhookFilter::NeedsDecision => match kind {
                "lane.status_changed" => field("to") == Some("needs_decision"),
                "lane.reported" => field("status") == Some("needs_decision"),
                "lane.created" => field("status") == Some("needs_decision"),
                "decision.created" | "decision.overdue" => true,
                _ => false,
            },
            WebhookFilter::RequestCreated => matches!(kind, "request.created" | "decision.created"),
            WebhookFilter::RequestAnswered => matches!(
                kind,
                "request.completed"
                    | "request.cancelled"
                    | "decision.answered"
                    | "decision.expired"
            ),
            WebhookFilter::HoldConflict => matches!(kind, "hold.conflict" | "lane.hold_conflict"),
            WebhookFilter::LeaseExpiring => kind == "resource.lease_expiring",
            WebhookFilter::All => !matches!(
                kind.split('.').next(),
                Some("message" | "delivery" | "token" | "peer" | "node")
            ),
        }
    }
}

impl std::str::FromStr for WebhookFilter {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        WebhookFilter::ALL
            .into_iter()
            .find(|f| f.as_str() == s.trim())
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "unknown webhook filter {s:?}; expected one of {}",
                    WebhookFilter::ALL.map(|f| f.as_str()).join(", ")
                ))
            })
    }
}

/// A webhook subscription. The signing secret is shown only when it is created.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Webhook {
    pub webhook_id: String,
    pub url: String,
    pub filters: Vec<WebhookFilter>,
    /// Projects (path or slug globs) whose events this webhook may carry; `None` = every
    /// project (local only).
    pub project_globs: Option<Vec<String>>,
    pub description: Option<String>,
    /// The agent whose token created it, when created remotely.
    pub created_by: Option<String>,
    pub created_at: String,
    pub pending: u32,
    pub delivered: u32,
    pub dead: u32,
    /// Set when the webhook stopped firing because its creating token was revoked or
    /// expired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
}

/// Returned once, at creation: the subscription plus its signing secret.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct WebhookCreated {
    pub webhook: Webhook,
    /// HMAC-SHA256 key for `X-Another-Signature`. Not shown again.
    pub secret: String,
}

/// Input for [`Store::create_webhook`].
#[derive(Debug, Clone, Default)]
pub struct NewWebhook {
    pub url: String,
    pub filters: Vec<WebhookFilter>,
    /// Required for remote callers (defaults to the token's globs); `None` locally means
    /// every project.
    pub project_globs: Option<Vec<String>>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    Pending,
    Delivered,
    Dead,
}

impl DeliveryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            DeliveryStatus::Pending => "pending",
            DeliveryStatus::Delivered => "delivered",
            DeliveryStatus::Dead => "dead",
        }
    }
    fn parse(s: &str) -> DeliveryStatus {
        match s {
            "delivered" => DeliveryStatus::Delivered,
            "dead" => DeliveryStatus::Dead,
            _ => DeliveryStatus::Pending,
        }
    }
}

/// One queued, delivered or dead delivery.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct WebhookDelivery {
    pub delivery_id: String,
    pub webhook_id: String,
    pub event_id: String,
    pub kind: String,
    pub filter: String,
    pub status: DeliveryStatus,
    pub attempts: u32,
    pub next_attempt_at: Option<String>,
    pub last_status: Option<u16>,
    pub last_error: Option<String>,
    pub created_at: String,
    pub delivered_at: Option<String>,
}

/// A delivery claimed for sending: everything the sender needs.
#[derive(Debug, Clone)]
pub struct OutboundDelivery {
    pub delivery_id: String,
    pub webhook_id: String,
    pub url: String,
    pub secret: String,
    pub kind: String,
    pub body: String,
    pub attempt: u32,
}

impl OutboundDelivery {
    /// Headers to send, signed at `timestamp_ms`.
    pub fn headers(&self, timestamp_ms: i64) -> Vec<(&'static str, String)> {
        vec![
            ("Content-Type", "application/json".into()),
            ("X-Another-Event", self.kind.clone()),
            ("X-Another-Delivery", self.delivery_id.clone()),
            ("X-Another-Timestamp", timestamp_ms.to_string()),
            (
                "X-Another-Signature",
                signature(&self.secret, timestamp_ms, &self.body),
            ),
        ]
    }
}

/// `sha256=<hex HMAC-SHA256(secret, "<timestamp>.<body>")>`, the `X-Another-Signature` value.
pub fn signature(secret: &str, timestamp_ms: i64, body: &str) -> String {
    let msg = format!("{timestamp_ms}.{body}");
    format!(
        "sha256={}",
        crate::crypto::hmac_sha256_hex(secret.as_bytes(), msg.as_bytes())
    )
}

/// Delay before retry `n` (1-based, counting failed attempts). After the last, dead.
const BACKOFF_MS: [i64; 6] = [10_000, 30_000, 120_000, 600_000, 3_600_000, 21_600_000];
/// Attempts before a delivery is dead.
pub const MAX_DELIVERY_ATTEMPTS: u32 = 7;
/// How long a claimed delivery is reserved for its sender (so two processes never send it twice).
const CLAIM_MS: i64 = 60_000;
/// Events considered per webhook per queueing pass.
const QUEUE_BATCH: u32 = 500;

/// Check a webhook URL: `https://`, or `http://` to loopback only.
pub fn validate_webhook_url(url: &str) -> Result<()> {
    let url = url.trim();
    let bad = |why: &str| Err(Error::Invalid(format!("webhook url {url:?}: {why}")));
    let (scheme, rest) = match url.split_once("://") {
        Some(x) => x,
        None => return bad("expected https://host/path"),
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') || url.chars().any(char::is_whitespace) {
        return bad("expected https://host/path (no credentials in the URL)");
    }
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    match scheme.to_ascii_lowercase().as_str() {
        "https" => Ok(()),
        "http"
            if matches!(
                host.to_ascii_lowercase().as_str(),
                "127.0.0.1" | "localhost" | "::1"
            ) =>
        {
            Ok(())
        }
        "http" => bad("plain http is only allowed to loopback (127.0.0.1, localhost, [::1])"),
        _ => bad("scheme must be https"),
    }
}

/// A scope that only knows project globs: what a webhook may see.
/// The scope a webhook delivers within: its project globs, plus (for webhooks a token
/// created) that token's agent, so project-less events follow the same own-agent rule as
/// the stream and long-poll (D38).
fn hook_scope(globs: &[String], agent_id: Option<&str>) -> TokenScope {
    TokenScope {
        token_id: String::new(),
        // "\0" never equals a real agent id, so local webhooks get project events only.
        agent_id: agent_id.unwrap_or("\u{0}").to_string(),
        agent: "\u{0}".into(),
        project_globs: globs.to_vec(),
    }
}

/// Disable webhooks whose creating token was revoked or has expired: they stop firing
/// and their pending deliveries die. Returns how many were disabled.
fn disable_orphaned_tx(tx: &crate::store::Tx<'_>) -> Result<usize> {
    let mut stmt = tx.prepare(
        "SELECT w.webhook_id,
                CASE WHEN t.token_id IS NULL OR t.revoked_at IS NOT NULL THEN 'token revoked'
                     ELSE 'token expired' END
         FROM webhooks w LEFT JOIN tokens t ON t.token_id = w.token_id
         WHERE w.node_id = ?1 AND w.removed_at IS NULL AND w.disabled_at IS NULL
           AND w.token_id IS NOT NULL
           AND (t.token_id IS NULL OR t.revoked_at IS NOT NULL
                OR (t.expires_at IS NOT NULL AND t.expires_at <= ?2))",
    )?;
    let dead = stmt
        .query_map(params![tx.node_id, tx.now], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, reason) in &dead {
        tx.execute(
            "UPDATE webhooks SET disabled_at = ?2, disabled_reason = ?3 WHERE webhook_id = ?1",
            params![id, tx.now, reason],
        )?;
        tx.execute(
            "UPDATE webhook_deliveries SET status = 'dead', last_error = ?2
             WHERE webhook_id = ?1 AND status = 'pending'",
            params![id, format!("webhook disabled: {reason}")],
        )?;
    }
    Ok(dead.len())
}

struct HookRow {
    webhook_id: String,
    url: String,
    filters: Vec<WebhookFilter>,
    globs: Option<Vec<String>>,
    description: Option<String>,
    token_id: Option<String>,
    created_by: Option<String>,
    created_at: i64,
    cursor_seq: i64,
    disabled_at: Option<i64>,
    disabled_reason: Option<String>,
    /// The creating token's agent (own-agent rule for project-less events).
    token_agent_id: Option<String>,
}

const HOOK_COLS: &str = "webhook_id, url, filters, project_globs, description, token_id,
    created_by, created_at, cursor_seq, disabled_at, disabled_reason,
    (SELECT t.agent_id FROM tokens t WHERE t.token_id = webhooks.token_id)";

fn hook_row(r: &Row<'_>) -> rusqlite::Result<HookRow> {
    let filters: String = r.get(2)?;
    let globs: Option<String> = r.get(3)?;
    Ok(HookRow {
        webhook_id: r.get(0)?,
        url: r.get(1)?,
        filters: serde_json::from_str(&filters).unwrap_or_default(),
        globs: globs.and_then(|g| serde_json::from_str(&g).ok()),
        description: r.get(4)?,
        token_id: r.get(5)?,
        created_by: r.get(6)?,
        created_at: r.get(7)?,
        cursor_seq: r.get(8)?,
        disabled_at: r.get(9)?,
        disabled_reason: r.get(10)?,
        token_agent_id: r.get(11)?,
    })
}

fn hydrate(conn: &Connection, h: HookRow) -> Result<Webhook> {
    let count = |status: &str| -> Result<u32> {
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM webhook_deliveries WHERE webhook_id = ?1 AND status = ?2",
            params![h.webhook_id, status],
            |r| r.get(0),
        )?)
    };
    Ok(Webhook {
        pending: count("pending")?,
        delivered: count("delivered")?,
        dead: count("dead")?,
        webhook_id: h.webhook_id,
        url: h.url,
        filters: h.filters,
        project_globs: h.globs,
        description: h.description,
        created_by: h.created_by,
        created_at: fmt_ts(h.created_at),
        disabled_at: h.disabled_at.map(fmt_ts),
        disabled_reason: h.disabled_reason,
    })
}

fn live_hook(conn: &Connection, id: &str) -> Result<HookRow> {
    conn.query_row(
        &format!("SELECT {HOOK_COLS} FROM webhooks WHERE webhook_id = ?1 AND removed_at IS NULL"),
        [id],
        hook_row,
    )
    .optional()?
    .ok_or_else(|| Error::NotFound(format!("no webhook {id:?}")))
}

/// Remote callers may only manage webhooks their own token created.
fn check_owner(h: &HookRow, scope: Option<&TokenScope>) -> Result<()> {
    match scope {
        Some(s) if h.token_id.as_deref() != Some(s.token_id.as_str()) => Err(Error::Forbidden(
            format!("webhook {} was not created by this token", h.webhook_id),
        )),
        _ => Ok(()),
    }
}

fn delivery_row(r: &Row<'_>) -> rusqlite::Result<WebhookDelivery> {
    let status = DeliveryStatus::parse(&r.get::<_, String>(5)?);
    Ok(WebhookDelivery {
        delivery_id: r.get(0)?,
        webhook_id: r.get(1)?,
        event_id: r.get(2)?,
        kind: r.get(3)?,
        filter: r.get(4)?,
        status,
        attempts: r.get(6)?,
        next_attempt_at: if status == DeliveryStatus::Pending {
            Some(fmt_ts(r.get(7)?))
        } else {
            None
        },
        last_status: r.get(8)?,
        last_error: r.get(9)?,
        created_at: fmt_ts(r.get(10)?),
        delivered_at: r.get::<_, Option<i64>>(11)?.map(fmt_ts),
    })
}

const DELIVERY_COLS: &str = "delivery_id, webhook_id, event_id, kind, filter, status, attempts,
    next_attempt_at, last_status, last_error, created_at, delivered_at";

impl Store {
    /// Create a webhook subscription. New subscriptions see events from now on.
    pub fn create_webhook(
        &self,
        input: NewWebhook,
        scope: Option<&TokenScope>,
    ) -> Result<WebhookCreated> {
        validate_webhook_url(&input.url)?;
        if input.filters.is_empty() {
            return Err(Error::Invalid(format!(
                "choose at least one event filter: {}",
                WebhookFilter::ALL.map(|f| f.as_str()).join(", ")
            )));
        }
        let mut filters: Vec<WebhookFilter> = Vec::new();
        for f in input.filters {
            if !filters.contains(&f) {
                filters.push(f);
            }
        }
        let globs = match (scope, input.project_globs) {
            (Some(s), None) => Some(s.project_globs.clone()),
            (Some(s), Some(g)) => {
                if let Some(extra) = g.iter().find(|x| !s.project_globs.contains(x)) {
                    return Err(Error::Forbidden(format!(
                        "project glob {extra:?} is not one of this token's globs ({})",
                        s.project_globs.join(", ")
                    )));
                }
                Some(g)
            }
            (None, g) => g,
        };
        if let Some(g) = &globs {
            if g.is_empty() || g.iter().any(|x| x.trim().is_empty()) {
                return Err(Error::Invalid("project globs must not be empty".into()));
            }
        }
        let secret = format!("whsec_{}", random_hex(32)?);
        let webhook_id = new_id();
        self.write(|tx| {
            let cursor: i64 =
                tx.query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))?;
            tx.execute(
                "INSERT INTO webhooks (webhook_id, node_id, url, secret, filters, project_globs,
                   description, token_id, created_by, created_at, cursor_seq)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    webhook_id,
                    tx.node_id,
                    input.url.trim(),
                    secret,
                    serde_json::to_string(&filters)?,
                    globs.as_ref().map(serde_json::to_string).transpose()?,
                    input.description,
                    scope.map(|s| s.token_id.clone()),
                    scope.map(|s| s.agent.clone()),
                    tx.now,
                    cursor
                ],
            )?;
            let h = live_hook(tx, &webhook_id)?;
            Ok(WebhookCreated {
                webhook: hydrate(tx, h)?,
                secret,
            })
        })
    }

    /// Whether any live webhook exists (a cheap read, so idle workers take no write lock).
    pub fn has_webhooks(&self) -> Result<bool> {
        self.read(|conn| {
            Ok(conn
                .query_row(
                    "SELECT 1 FROM webhooks WHERE removed_at IS NULL LIMIT 1",
                    [],
                    |_| Ok(()),
                )
                .optional()?
                .is_some())
        })
    }

    /// Live webhooks (remote callers see only their token's).
    pub fn webhooks(&self, scope: Option<&TokenScope>) -> Result<Vec<Webhook>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {HOOK_COLS} FROM webhooks WHERE removed_at IS NULL
                   AND (?1 IS NULL OR token_id = ?1)
                 ORDER BY created_at"
            ))?;
            let rows = stmt
                .query_map([scope.map(|s| s.token_id.as_str())], hook_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().map(|h| hydrate(conn, h)).collect()
        })
    }

    /// Remove a webhook. Pending deliveries are dropped; history is kept.
    pub fn remove_webhook(&self, webhook_id: &str, scope: Option<&TokenScope>) -> Result<Webhook> {
        self.write(|tx| {
            let h = live_hook(tx, webhook_id)?;
            check_owner(&h, scope)?;
            tx.execute(
                "UPDATE webhooks SET removed_at = ?2 WHERE webhook_id = ?1",
                params![webhook_id, tx.now],
            )?;
            tx.execute(
                "UPDATE webhook_deliveries SET status = 'dead', last_error = 'webhook removed'
                 WHERE webhook_id = ?1 AND status = 'pending'",
                [webhook_id],
            )?;
            hydrate(tx, h)
        })
    }

    /// Deliveries, newest first, optionally only one webhook's or one status.
    pub fn webhook_deliveries(
        &self,
        webhook_id: Option<&str>,
        status: Option<DeliveryStatus>,
        limit: u32,
    ) -> Result<Vec<WebhookDelivery>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {DELIVERY_COLS} FROM webhook_deliveries
                 WHERE (?1 IS NULL OR webhook_id = ?1) AND (?2 IS NULL OR status = ?2)
                 ORDER BY created_at DESC, delivery_id DESC LIMIT ?3"
            ))?;
            let rows = stmt
                .query_map(
                    params![webhook_id, status.map(|s| s.as_str()), limit.max(1)],
                    delivery_row,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Re-queue a dead delivery for immediate sending.
    pub fn retry_delivery(&self, delivery_id: &str) -> Result<WebhookDelivery> {
        self.write(|tx| {
            let (status, webhook_id): (String, String) = tx
                .query_row(
                    "SELECT status, webhook_id FROM webhook_deliveries WHERE delivery_id = ?1",
                    [delivery_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?
                .ok_or_else(|| Error::NotFound(format!("no delivery {delivery_id:?}")))?;
            if status != "dead" {
                return Err(Error::Conflict(format!(
                    "delivery {delivery_id} is {status}; only dead deliveries can be retried"
                )));
            }
            live_hook(tx, &webhook_id)?;
            tx.execute(
                "UPDATE webhook_deliveries SET status = 'pending', attempts = 0,
                   next_attempt_at = ?2 WHERE delivery_id = ?1",
                params![delivery_id, tx.now],
            )?;
            Ok(tx.query_row(
                &format!("SELECT {DELIVERY_COLS} FROM webhook_deliveries WHERE delivery_id = ?1"),
                [delivery_id],
                delivery_row,
            )?)
        })
    }

    /// Turn new events into deliveries for every live webhook. Each event is considered
    /// once per webhook (a cursor per webhook). Returns how many deliveries were queued.
    pub fn queue_webhook_deliveries(&self) -> Result<usize> {
        self.write(|tx| {
            disable_orphaned_tx(tx)?;
            let mut stmt = tx.prepare(&format!(
                "SELECT {HOOK_COLS} FROM webhooks WHERE node_id = ?1 AND removed_at IS NULL
                   AND disabled_at IS NULL"
            ))?;
            let hooks = stmt
                .query_map([tx.node_id], hook_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut queued = 0;
            for h in hooks {
                let mut stmt = tx.prepare(
                    "SELECT seq, event_id, node_id, ts, actor, kind, payload FROM events
                     WHERE seq > ?1 ORDER BY seq LIMIT ?2",
                )?;
                let rows = stmt
                    .query_map(params![h.cursor_seq, QUEUE_BATCH], |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            EventRow {
                                event_id: r.get(1)?,
                                node_id: r.get(2)?,
                                ts: r.get(3)?,
                                actor: r.get(4)?,
                                kind: r.get(5)?,
                                payload: r.get(6)?,
                            },
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let Some(last) = rows.last().map(|(seq, _)| *seq) else {
                    continue;
                };
                let scope = h
                    .globs
                    .as_deref()
                    .map(|g| hook_scope(g, h.token_agent_id.as_deref()));
                for (_, row) in rows {
                    let payload: Value = serde_json::from_str(&row.payload).unwrap_or(Value::Null);
                    let Some(filter) = h.filters.iter().find(|f| f.matches(&row.kind, &payload))
                    else {
                        continue;
                    };
                    if let Some(scope) = &scope {
                        if !event_visible(
                            tx,
                            scope,
                            &row.node_id,
                            &row.actor,
                            &row.kind,
                            &row.payload,
                        )? {
                            continue;
                        }
                    }
                    let delivery_id = new_id();
                    let event_id = row.event_id.clone();
                    let kind = row.kind.clone();
                    let event = item(tx, row)?;
                    let body = json!({
                        "delivery_id": delivery_id,
                        "webhook_id": h.webhook_id,
                        "filter": filter.as_str(),
                        "event": event,
                        "data": payload,
                    })
                    .to_string();
                    queued += tx.execute(
                        "INSERT OR IGNORE INTO webhook_deliveries (delivery_id, webhook_id,
                           event_id, kind, filter, body, status, next_attempt_at, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?7)",
                        params![
                            delivery_id,
                            h.webhook_id,
                            event_id,
                            kind,
                            filter.as_str(),
                            body,
                            tx.now
                        ],
                    )?;
                }
                tx.execute(
                    "UPDATE webhooks SET cursor_seq = ?2 WHERE webhook_id = ?1",
                    params![h.webhook_id, last],
                )?;
            }
            Ok(queued)
        })
    }

    /// Queue a `webhook.test` delivery (not an event in the log) and claim it for sending.
    pub fn queue_test_delivery(&self, webhook_id: &str) -> Result<OutboundDelivery> {
        let delivery_id = new_id();
        self.write(|tx| {
            let h = live_hook(tx, webhook_id)?;
            if let Some(reason) = &h.disabled_reason {
                return Err(Error::Conflict(format!(
                    "webhook {} is disabled ({reason}); create a new one",
                    h.webhook_id
                )));
            }
            let body = json!({
                "delivery_id": delivery_id,
                "webhook_id": h.webhook_id,
                "filter": "test",
                "event": { "kind": "webhook.test", "ts": fmt_ts(tx.now), "summary": "test delivery" },
                "data": {},
            })
            .to_string();
            tx.execute(
                "INSERT INTO webhook_deliveries (delivery_id, webhook_id, event_id, kind, filter,
                   body, status, next_attempt_at, created_at)
                 VALUES (?1, ?2, ?3, 'webhook.test', 'test', ?4, 'pending', ?5, ?6)",
                params![
                    delivery_id,
                    h.webhook_id,
                    format!("test-{delivery_id}"),
                    body,
                    tx.now + CLAIM_MS,
                    tx.now
                ],
            )?;
            let secret: String = tx.query_row(
                "SELECT secret FROM webhooks WHERE webhook_id = ?1",
                [&h.webhook_id],
                |r| r.get(0),
            )?;
            Ok(OutboundDelivery {
                delivery_id: delivery_id.clone(),
                webhook_id: h.webhook_id,
                url: h.url,
                secret,
                kind: "webhook.test".into(),
                body,
                attempt: 1,
            })
        })
    }

    /// Claim up to `limit` due deliveries for sending. Claimed deliveries are reserved for a
    /// minute, so another process running a delivery worker will not send them too.
    pub fn claim_due_deliveries(&self, limit: u32) -> Result<Vec<OutboundDelivery>> {
        self.write(|tx| {
            let mut stmt = tx.prepare(
                "SELECT d.delivery_id, d.webhook_id, w.url, w.secret, d.kind, d.body, d.attempts
                 FROM webhook_deliveries d JOIN webhooks w ON w.webhook_id = d.webhook_id
                 WHERE d.status = 'pending' AND d.next_attempt_at <= ?1 AND w.removed_at IS NULL
                   AND w.disabled_at IS NULL
                 ORDER BY d.next_attempt_at, d.delivery_id LIMIT ?2",
            )?;
            let due = stmt
                .query_map(params![tx.now, limit], |r| {
                    Ok(OutboundDelivery {
                        delivery_id: r.get(0)?,
                        webhook_id: r.get(1)?,
                        url: r.get(2)?,
                        secret: r.get(3)?,
                        kind: r.get(4)?,
                        body: r.get(5)?,
                        attempt: r.get::<_, u32>(6)? + 1,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for d in &due {
                tx.execute(
                    "UPDATE webhook_deliveries SET next_attempt_at = ?2 WHERE delivery_id = ?1",
                    params![d.delivery_id, tx.now + CLAIM_MS],
                )?;
            }
            Ok(due)
        })
    }

    /// Record the outcome of sending a claimed delivery: an HTTP status, or a network
    /// error. 2xx is delivered; anything else is retried with backoff, then dead.
    pub fn record_delivery(
        &self,
        delivery_id: &str,
        outcome: std::result::Result<u16, String>,
    ) -> Result<WebhookDelivery> {
        self.write(|tx| {
            let attempts: u32 = tx
                .query_row(
                    "SELECT attempts FROM webhook_deliveries WHERE delivery_id = ?1",
                    [delivery_id],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or_else(|| Error::NotFound(format!("no delivery {delivery_id:?}")))?;
            let attempts = attempts + 1;
            let (status, error) = match &outcome {
                Ok(code) if (200..300).contains(code) => (Some(*code), None),
                Ok(code) => (Some(*code), Some(format!("HTTP {code}"))),
                Err(e) => (None, Some(e.chars().take(500).collect::<String>())),
            };
            if error.is_none() {
                tx.execute(
                    "UPDATE webhook_deliveries SET status = 'delivered', attempts = ?2,
                       last_status = ?3, last_error = NULL, delivered_at = ?4
                     WHERE delivery_id = ?1",
                    params![delivery_id, attempts, status, tx.now],
                )?;
            } else if attempts >= MAX_DELIVERY_ATTEMPTS {
                tx.execute(
                    "UPDATE webhook_deliveries SET status = 'dead', attempts = ?2,
                       last_status = ?3, last_error = ?4 WHERE delivery_id = ?1",
                    params![delivery_id, attempts, status, error],
                )?;
            } else {
                let delay = BACKOFF_MS[(attempts as usize - 1).min(BACKOFF_MS.len() - 1)];
                tx.execute(
                    "UPDATE webhook_deliveries SET attempts = ?2, last_status = ?3,
                       last_error = ?4, next_attempt_at = ?5 WHERE delivery_id = ?1",
                    params![delivery_id, attempts, status, error, tx.now + delay],
                )?;
            }
            Ok(tx.query_row(
                &format!("SELECT {DELIVERY_COLS} FROM webhook_deliveries WHERE delivery_id = ?1"),
                [delivery_id],
                delivery_row,
            )?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        for ok in [
            "https://example.test/hook",
            "https://example.test:8443/a?b=c",
            "http://127.0.0.1:9000/x",
            "http://localhost/x",
            "http://[::1]:80/x",
        ] {
            assert!(validate_webhook_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "http://example.test/hook",
            "http://127.0.0.1.example.test/x",
            "ftp://example.test",
            "https://user:pw@example.test/x",
            "example.test/x",
            "https:///x",
        ] {
            assert!(validate_webhook_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn filters() {
        let to = |s: &str| json!({ "to": s });
        assert!(WebhookFilter::NeedsDecision.matches("lane.status_changed", &to("needs_decision")));
        assert!(!WebhookFilter::NeedsDecision.matches("lane.status_changed", &to("working")));
        assert!(WebhookFilter::LaneStatus.matches("lane.status_changed", &to("working")));
        assert!(!WebhookFilter::All.matches("message.sent", &json!({})));
        assert!(WebhookFilter::All.matches("hold.conflict", &json!({})));
        assert_eq!(
            "lease_expiring".parse::<WebhookFilter>().unwrap(),
            WebhookFilter::LeaseExpiring
        );
        assert!("nope".parse::<WebhookFilter>().is_err());
    }

    #[test]
    fn signature_format() {
        let s = signature("whsec_x", 1700000000000, "{}");
        assert!(s.starts_with("sha256="));
        assert_eq!(s.len(), 7 + 64);
        assert_ne!(s, signature("whsec_y", 1700000000000, "{}"));
    }
}
