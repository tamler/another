//! Push (docs/design/02-push.md): the webhook delivery worker, the long-poll helper shared by
//! `GET /events/poll` and MCP `events_poll`, and the `GET /events/stream` SSE endpoint. All
//! reads are filtered by the caller's token scope.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::time::{Duration, Instant};

use another_core::{
    ActivityFeed, ActivityItem, ActivityKind, ActivityQuery, AuditRecord, OutboundDelivery, Since,
    Store, TokenScope, WebhookDelivery,
};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;
// ureq's `unversioned` API may change between minor releases; Cargo.lock pins the version.
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

use crate::remote::{audit, RemotePeer};

/// How often the delivery worker looks for new events and due deliveries.
pub const DELIVERY_EVERY: Duration = Duration::from_secs(2);
/// Deliveries sent per worker pass.
const DELIVERY_BATCH: u32 = 20;
/// How often streams and long-polls re-read the log.
const POLL_EVERY: Duration = Duration::from_secs(1);
/// How often an open stream re-checks its bearer token (so revoking it ends the stream).
const RECHECK_TOKEN_EVERY: Duration = Duration::from_secs(30);
/// Longest `GET /events/poll` wait.
pub const MAX_HTTP_POLL_SECONDS: u64 = 60;
/// Longest MCP `events_poll` wait.
pub const MAX_MCP_POLL_SECONDS: u64 = 30;

// ---------------------------------------------------------------------------
// Webhook delivery

fn agent_config() -> ureq::config::ConfigBuilder<ureq::typestate::AgentScope> {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        // A redirect could point an https hook at a plain-http or internal address.
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(10)))
}

fn http_agent(url: &str) -> ureq::Agent {
    let loopback = ["http://127.0.0.1", "http://localhost", "http://[::1]"]
        .iter()
        .any(|p| url.starts_with(p));
    let mut config = agent_config();
    if loopback {
        config = config.proxy(None);
    }
    ureq::Agent::new_with_config(config.build())
}

/// Checks every address a name resolves to, inside the connection's own lookup: a separate
/// check beforehand could be answered differently by DNS (rebinding). One refused address
/// refuses them all, so a mixed answer can't fall through to it.
#[derive(Debug)]
struct RefuseHubAddresses<R>(R);

impl<R: Resolver> Resolver for RefuseHubAddresses<R> {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let addrs = self.0.resolve(uri, config, timeout)?;
        if addrs
            .iter()
            .any(|a| another_core::is_refused_webhook_ip(a.ip()))
        {
            return Err(ureq::Error::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "webhook host resolves to an address that is not allowed for a token's webhook",
            )));
        }
        Ok(addrs)
    }
}

/// The agent for a token's webhook. It goes direct: through a proxy, the proxy would resolve
/// the name and this check would see only the proxy's address.
fn restricted_agent(resolver: impl Resolver) -> ureq::Agent {
    ureq::Agent::with_parts(
        agent_config().proxy(None).build(),
        DefaultConnector::default(),
        RefuseHubAddresses(resolver),
    )
}

/// POST one delivery, signed now. Returns the HTTP status, or a network error.
pub fn send(d: &OutboundDelivery) -> Result<u16, String> {
    let agent = if d.restricted {
        another_core::validate_remote_webhook_host(&d.url).map_err(|e| e.to_string())?;
        restricted_agent(DefaultResolver::default())
    } else {
        http_agent(&d.url)
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.as_millis() as i64)
        .unwrap_or_default();
    let mut req = agent
        .post(&d.url)
        .header("User-Agent", concat!("another/", env!("CARGO_PKG_VERSION")));
    for (k, v) in d.headers(now) {
        req = req.header(k, v);
    }
    match req.send(d.body.as_bytes()) {
        Ok(resp) => Ok(resp.status().as_u16()),
        Err(e) => Err(e.to_string()),
    }
}

/// Send a claimed delivery and record the outcome.
pub fn deliver(store: &Store, d: &OutboundDelivery) -> another_core::Result<WebhookDelivery> {
    let outcome = send(d);
    if let Err(e) = &outcome {
        tracing::warn!(webhook = %d.webhook_id, delivery = %d.delivery_id, error = %e, "webhook delivery failed");
    }
    store.record_delivery(&d.delivery_id, outcome)
}

/// One worker pass: queue new events, then send what is due. Returns how many deliveries
/// were attempted. Without webhooks it only reads. (`lease_expiring` events come from the
/// sweeper, which runs alongside.)
pub fn deliver_once(store: &Store) -> another_core::Result<usize> {
    if !store.has_webhooks()? {
        return Ok(0);
    }
    store.queue_webhook_deliveries()?;
    // Claim one delivery right before sending it: a claim lasts a minute, and a batch of
    // slow receivers could otherwise outlast the claims of the later deliveries, letting a
    // second `serve` process send them again.
    let mut sent = 0;
    while sent < DELIVERY_BATCH as usize {
        let Some(d) = store.claim_due_deliveries(1)?.into_iter().next() else {
            break;
        };
        deliver(store, &d)?;
        sent += 1;
    }
    Ok(sent)
}

/// Run the delivery worker every [`DELIVERY_EVERY`] until cancelled.
pub fn spawn_delivery_worker(store: Store, ct: CancellationToken) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(DELIVERY_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ct.cancelled() => break,
                _ = tick.tick() => {
                    let store = store.clone();
                    match tokio::task::spawn_blocking(move || deliver_once(&store)).await {
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => tracing::warn!(error = %e, "webhook worker failed"),
                        Err(e) => tracing::warn!(error = %e, "webhook worker task failed"),
                    }
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Long-poll and stream

/// Parse `kinds=mail,lane` into categories.
pub fn parse_kinds(raw: Option<&str>) -> another_core::Result<Vec<ActivityKind>> {
    raw.unwrap_or_default()
        .split(',')
        .filter(|k| !k.trim().is_empty())
        .map(|k| k.trim().parse::<ActivityKind>())
        .collect()
}

/// Where a push read starts: the given cursor or duration, else "from now on".
pub fn start_cursor(store: &Store, since: Option<&str>) -> another_core::Result<Since> {
    match since.map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => Since::parse(raw, store.now()),
        None => Ok(match store.end_cursor()? {
            Some(id) => Since::After(id),
            None => Since::Time(store.now()),
        }),
    }
}

/// Wait up to `timeout` for events after `since` that `scope` may see.
pub async fn long_poll(
    store: Store,
    scope: Option<TokenScope>,
    since: Option<String>,
    kinds: Vec<ActivityKind>,
    timeout: Duration,
) -> another_core::Result<ActivityFeed> {
    let deadline = Instant::now() + timeout;
    let s = store.clone();
    let mut cursor = tokio::task::spawn_blocking(move || start_cursor(&s, since.as_deref()))
        .await
        .map_err(|e| another_core::Error::Invalid(e.to_string()))??;
    loop {
        let (s, sc, k, c) = (store.clone(), scope.clone(), kinds.clone(), cursor.clone());
        let feed = tokio::task::spawn_blocking(move || {
            s.activity_scoped(
                ActivityQuery {
                    since: Some(c),
                    limit: 100,
                    kinds: k,
                },
                sc.as_ref(),
            )
        })
        .await
        .map_err(|e| another_core::Error::Invalid(e.to_string()))??;
        if let Some(next) = &feed.next_since {
            cursor = Since::After(next.clone());
        }
        if !feed.items.is_empty() || Instant::now() >= deadline {
            let next_since = feed.next_since.or(match &cursor {
                Since::After(id) => Some(id.clone()),
                Since::Time(_) => None,
            });
            return Ok(ActivityFeed {
                items: feed.items,
                next_since,
            });
        }
        tokio::time::sleep(POLL_EVERY.min(deadline.saturating_duration_since(Instant::now())))
            .await;
    }
}

#[derive(Debug, Deserialize)]
pub struct PollParams {
    pub since: Option<String>,
    /// Comma-separated categories.
    pub kinds: Option<String>,
    /// Seconds to wait for an event (default 30, at most 60).
    pub timeout: Option<u64>,
}

async fn audit_peer(
    store: &Store,
    peer: &Option<RemotePeer>,
    op: &str,
    outcome: &str,
    detail: String,
) {
    if let Some(peer) = peer {
        audit(
            store,
            AuditRecord {
                token_id: Some(peer.scope.token_id.clone()),
                agent: Some(peer.scope.agent.clone()),
                operation: op.into(),
                outcome: outcome.into(),
                detail: Some(detail),
                remote: peer.remote.clone(),
            },
        )
        .await;
    }
}

fn error_response(e: &another_core::Error) -> Response {
    let status = match e.code() {
        "not_found" => StatusCode::NOT_FOUND,
        "forbidden" => StatusCode::FORBIDDEN,
        _ => StatusCode::BAD_REQUEST,
    };
    (status, axum::Json(json!({ "error": e.body() }))).into_response()
}

/// `GET /events/poll?since=&kinds=&timeout=30` — long-poll JSON.
pub async fn poll(
    State(store): State<Store>,
    peer: Option<axum::Extension<RemotePeer>>,
    Query(params): Query<PollParams>,
) -> Response {
    let peer = peer.map(|p| p.0);
    let detail = format!(
        "since={:?} kinds={:?} timeout={:?}",
        params.since, params.kinds, params.timeout
    );
    let kinds = match parse_kinds(params.kinds.as_deref()) {
        Ok(k) => k,
        Err(e) => {
            audit_peer(&store, &peer, "GET /events/poll", "error", detail).await;
            return error_response(&e);
        }
    };
    let timeout = Duration::from_secs(params.timeout.unwrap_or(30).min(MAX_HTTP_POLL_SECONDS));
    let scope = peer.as_ref().map(|p| p.scope.clone());
    let result = long_poll(store.clone(), scope, params.since, kinds, timeout).await;
    let outcome = if result.is_ok() { "ok" } else { "error" };
    audit_peer(&store, &peer, "GET /events/poll", outcome, detail).await;
    match result {
        Ok(feed) => (StatusCode::OK, axum::Json(json!(feed))).into_response(),
        Err(e) => error_response(&e),
    }
}

#[derive(Debug, Deserialize)]
pub struct StreamParams {
    pub since: Option<String>,
    /// Comma-separated categories.
    pub kinds: Option<String>,
}

struct StreamState {
    store: Store,
    scope: Option<TokenScope>,
    kinds: Vec<ActivityKind>,
    cursor: Since,
    pending: VecDeque<ActivityItem>,
    /// The caller's bearer token (remote streams only), re-verified periodically.
    bearer: Option<String>,
    checked_at: Instant,
}

fn sse_event(item: &ActivityItem) -> SseEvent {
    SseEvent::default()
        .id(item.event_id.clone())
        .event(item.kind.clone())
        .data(serde_json::to_string(item).unwrap_or_default())
}

/// `GET /events/stream?since=&kinds=` — Server-Sent Events. Resumes from `Last-Event-ID`.
pub async fn stream(
    State(store): State<Store>,
    peer: Option<axum::Extension<RemotePeer>>,
    headers: HeaderMap,
    Query(params): Query<StreamParams>,
) -> Response {
    let peer = peer.map(|p| p.0);
    let since = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or(params.since.clone());
    let detail = format!("since={since:?} kinds={:?}", params.kinds);
    let s = store.clone();
    let start = match parse_kinds(params.kinds.as_deref()) {
        Ok(kinds) => tokio::task::spawn_blocking(move || {
            start_cursor(&s, since.as_deref()).map(|c| (c, kinds))
        })
        .await
        .unwrap_or_else(|e| Err(another_core::Error::Invalid(e.to_string()))),
        Err(e) => Err(e),
    };
    let (cursor, kinds) = match start {
        Ok(x) => x,
        Err(e) => {
            audit_peer(&store, &peer, "GET /events/stream", "error", detail).await;
            return error_response(&e);
        }
    };
    audit_peer(&store, &peer, "GET /events/stream", "ok", detail).await;
    let bearer = peer.as_ref().and_then(|_| crate::remote::bearer(&headers));
    let state = StreamState {
        store,
        scope: peer.map(|p| p.scope),
        kinds,
        cursor,
        pending: VecDeque::new(),
        bearer,
        checked_at: Instant::now(),
    };
    let events = futures_util::stream::unfold(state, |mut st| async move {
        loop {
            if let Some(token) = st.bearer.clone() {
                if st.checked_at.elapsed() >= RECHECK_TOKEN_EVERY {
                    let s = st.store.clone();
                    let valid = tokio::task::spawn_blocking(move || s.verify_token(&token).is_ok())
                        .await
                        .unwrap_or(false);
                    if !valid {
                        return None;
                    }
                    st.checked_at = Instant::now();
                }
            }
            if let Some(item) = st.pending.pop_front() {
                return Some((Ok::<_, Infallible>(sse_event(&item)), st));
            }
            let (s, sc, k, c) = (
                st.store.clone(),
                st.scope.clone(),
                st.kinds.clone(),
                st.cursor.clone(),
            );
            let feed = tokio::task::spawn_blocking(move || {
                s.activity_scoped(
                    ActivityQuery {
                        since: Some(c),
                        limit: 100,
                        kinds: k,
                    },
                    sc.as_ref(),
                )
            })
            .await;
            match feed {
                Ok(Ok(feed)) => {
                    if let Some(next) = feed.next_since {
                        st.cursor = Since::After(next);
                    }
                    if feed.items.is_empty() {
                        tokio::time::sleep(POLL_EVERY).await;
                    }
                    st.pending.extend(feed.items);
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "event stream read failed");
                    return None;
                }
                Err(_) => return None,
            }
        }
    });
    Sse::new(events)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};

    use super::*;

    /// DNS that answers every name with fixed addresses, as a rebinding name would.
    #[derive(Debug)]
    struct FixedDns(Vec<SocketAddr>);

    impl Resolver for FixedDns {
        fn resolve(
            &self,
            _uri: &ureq::http::Uri,
            _config: &ureq::config::Config,
            _timeout: NextTimeout,
        ) -> Result<ResolvedSocketAddrs, ureq::Error> {
            let mut out = self.empty();
            for a in &self.0 {
                out.push(*a);
            }
            Ok(out)
        }
    }

    fn post(agent: &ureq::Agent, port: u16) -> Result<u16, String> {
        agent
            .post(format!("http://rebind.example.test:{port}/hook"))
            .send("{}")
            .map(|r| r.status().as_u16())
            .map_err(|e| e.to_string())
    }

    /// Finding 3: a harmless-looking name that resolves to this machine reaches nothing.
    #[test]
    fn a_name_resolving_to_a_hub_address_is_not_connected_to() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let local = listener.local_addr().unwrap();
        let public: SocketAddr = "93.184.216.34:443".parse().unwrap();

        for answer in [vec![local], vec![public, local]] {
            let agent = restricted_agent(FixedDns(answer));
            let err = post(&agent, local.port()).unwrap_err();
            assert!(err.contains("not allowed"), "{err}");
        }
        listener.set_nonblocking(true).unwrap();
        assert!(listener.accept().is_err(), "nothing connected");
        listener.set_nonblocking(false).unwrap();

        // The same answer without the guard does connect, so the check above means something.
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = conn.read(&mut buf).unwrap();
            conn.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                .unwrap();
        });
        let open = ureq::Agent::with_parts(
            agent_config().proxy(None).build(),
            DefaultConnector::default(),
            FixedDns(vec![local]),
        );
        assert_eq!(post(&open, local.port()), Ok(204));
        server.join().unwrap();
    }
}
