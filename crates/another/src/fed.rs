//! Federation transport: hub endpoints (`/fed/pair`, `/fed/push`, `/fed/pull`) and the spoke
//! client that dials out to them (`another pair join`, `another sync`).
//!
//! Node-to-node requests are signed with the sending node's key over method, path, time and
//! body; the hub checks them against the key it pinned at pairing.
//!
//! The hub's answers to a push and a pull are signed too. A spoke sends a fresh nonce with
//! each of those requests, the hub signs its `200` answer over its id, the spoke's id, the
//! nonce, the request's signature and the hash of the answer body, and the spoke checks
//! that against the hub key it pinned at pairing before it reads the body. So something
//! that answers in front of the hub can make a sync fail, and nothing else. There is no
//! unsigned mode: a spoke applies no unsigned answer, and a hub serves no push or pull
//! that carries no nonce.

use another_core::{
    is_loopback_host, AuditRecord, IngestReport, PairRequest, PairResponse, Peer, PullAnchors,
    PullResponse, PushRequest, Rejected, Skip, Store,
};
use anyhow::{anyhow, bail, Context, Result};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::json;

const H_NODE: &str = "x-another-node";
const H_TIME: &str = "x-another-time";
const H_SIG: &str = "x-another-signature";
const H_NONCE: &str = "x-another-nonce";
/// The `Upgrade` header of the `426` a hub gives a push or pull without a nonce. It names
/// the signed exchange; nothing reads it.
const SIGNED_SYNC: &str = "another-signed-sync/1";
/// What a hub tells a node whose push or pull carries no nonce. An older spoke prints it.
const NO_NONCE: &str = "this hub signs its answers and serves only nodes that ask for that, \
                        and this request did not. Update this node to the hub's build, then \
                        run `another sync`. Nothing was stored, nothing is lost, and the node \
                        does not need to pair again";
const NO_NONCE_AUDIT: &str = "no x-another-nonce header: the node runs an older build, or \
                              something on the path removed it";
const BAD_NONCE: &str = "x-another-nonce must be 32 lowercase hex characters";
/// An unsigned error body is quoted in the failure it causes as one line, cut to this many
/// characters: room for the longest refusal a hub sends, whole.
const ERROR_BODY_CHARS: usize = 600;
/// A hub address is shown cut to this many characters.
const ADDRESS_CHARS: usize = 200;
const BATCH: u32 = 500;
/// Largest `/fed/pair` request body a hub reads: the value the HTTP library applied by
/// default before D99, now stated. A pairing request is a few hundred bytes.
pub const PAIR_BODY_LIMIT_BYTES: usize = 2 * 1024 * 1024;
/// Largest pairing or push answer a spoke reads: the value the HTTP client applied by
/// default before D99, now stated. Neither answer carries events.
const REPLY_LIMIT_BYTES: u64 = 10 * 1024 * 1024;

/// The size bounds one side of a sync runs with (D99). A hub uses `push_body` and
/// `pull_budget`; a spoke uses `push_budget` and `pull_body`.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// A push batch is closed before its request body would pass this many bytes.
    pub push_budget: usize,
    /// Largest `/fed/push` request body the hub reads.
    pub push_body: usize,
    /// A pull page is closed before its response body would pass this many bytes.
    pub pull_budget: usize,
    /// Largest `/fed/pull` response body the spoke reads.
    pub pull_body: usize,
    /// The hub refuses a push whose node header names no paired spoke before it reads the
    /// body. A build before D99 read the body first.
    pub unknown_sender_refused_first: bool,
}

impl Limits {
    /// What this build runs with.
    pub const CURRENT: Limits = Limits {
        push_budget: another_core::PUSH_BATCH_BUDGET_BYTES,
        push_body: another_core::PUSH_BODY_LIMIT_BYTES,
        pull_budget: another_core::PULL_PAGE_BUDGET_BYTES,
        pull_body: another_core::PULL_BODY_LIMIT_BYTES,
        unknown_sender_refused_first: true,
    };

    /// What a build before D99 ran with: batches and pages bounded by count only, and the
    /// HTTP libraries' default body limits. Kept so the tests can run a new node against an
    /// older one.
    pub const BEFORE_D99: Limits = Limits {
        push_budget: usize::MAX,
        push_body: 2 * 1024 * 1024,
        pull_budget: usize::MAX,
        pull_body: 10 * 1024 * 1024,
        unknown_sender_refused_first: false,
    };

    /// One event at the cap fits a batch and a page, each budget fits under its limit, and
    /// an event that arrived by push fits a pull answer with as much room again for the
    /// node directory.
    pub fn consistent(&self) -> bool {
        // A real sequence number adds at most 19 bytes to the one digit the cap counts.
        let one_event = another_core::MAX_EVENT_WIRE_BYTES + 19 + another_core::PUSH_ENVELOPE_BYTES;
        one_event <= self.push_budget
            && one_event <= self.pull_budget
            && self.push_budget <= self.push_body
            && self.pull_budget <= self.pull_body
            && self.push_body.saturating_mul(2) <= self.pull_body
    }
}

/// Whether one side of a sync speaks the signed exchange. Every build from this one on
/// does, and nothing in the program chooses otherwise. Tests and the `test-support`
/// feature also allow unsigned exchange to simulate an older build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exchange {
    /// A spoke sends a nonce and applies only an answer the hub signed for its request; a
    /// hub refuses a push or pull without a nonce and signs every `200` answer.
    Signed,
    /// What a build before signed answers did: a spoke sends no nonce and checks no
    /// answer; a hub asks for no nonce, signs no answer and records no contact.
    #[cfg(any(test, feature = "test-support"))]
    Unsigned,
}

/// Routes a hub serves when started with `another serve --federation`.
pub fn router(store: Store) -> axum::Router {
    router_with(store, Limits::CURRENT)
}

/// [`router`] with the size bounds given. Each route that reads a body states its limit;
/// `/fed/pull` is a GET and reads none.
pub fn router_with(store: Store, limits: Limits) -> axum::Router {
    router_as(store, limits, Exchange::Signed)
}

/// [`router_with`] as a hub of the build `exchange` names.
pub fn router_as(store: Store, limits: Limits, exchange: Exchange) -> axum::Router {
    let mut push = axum::routing::post(push).layer(DefaultBodyLimit::max(limits.push_body));
    if limits.unknown_sender_refused_first {
        push = push.layer(axum::middleware::from_fn_with_state(
            store.clone(),
            known_sender,
        ));
    }
    axum::Router::new()
        .route(
            "/fed/pair",
            axum::routing::post(pair).layer(DefaultBodyLimit::max(PAIR_BODY_LIMIT_BYTES)),
        )
        .route("/fed/push", push)
        .route("/fed/pull", axum::routing::get(pull))
        .layer(axum::Extension(limits))
        .layer(axum::Extension(exchange))
        .with_state(store)
}

fn error(status: StatusCode, e: &another_core::Error) -> Response {
    (status, axum::Json(json!({ "error": e.body() }))).into_response()
}

fn status_for(e: &another_core::Error) -> StatusCode {
    match e.code() {
        "forbidden" => StatusCode::FORBIDDEN,
        "not_found" => StatusCode::NOT_FOUND,
        "invalid" => StatusCode::BAD_REQUEST,
        "conflict" | "ambiguous" => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> another_core::Result<T> + Send + 'static,
) -> another_core::Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|e| Err(another_core::Error::Invalid(format!("task failed: {e}"))))
}

async fn record(
    store: &Store,
    node: Option<String>,
    operation: &str,
    outcome: &str,
    detail: String,
) {
    record_checked(store, node, operation, outcome, detail, true).await;
}

async fn record_unauthenticated(
    store: &Store,
    node: Option<String>,
    operation: &str,
    outcome: &str,
    detail: String,
) {
    record_checked(store, node, operation, outcome, detail, false).await;
}

async fn record_checked(
    store: &Store,
    node: Option<String>,
    operation: &str,
    outcome: &str,
    detail: String,
    authenticated: bool,
) {
    let entry = AuditRecord {
        token_id: None,
        // The id is whatever the caller sent, so it is written only when it is an id.
        agent: node.map(|n| match another_core::validate_node_id(&n) {
            Ok(()) => format!("node:{n}"),
            Err(_) => "node:(invalid id)".to_string(),
        }),
        operation: operation.to_string(),
        outcome: outcome.to_string(),
        detail: Some(detail),
        remote: None,
    };
    if !authenticated {
        crate::remote::unauthenticated_audit(store, entry).await;
    } else {
        crate::remote::audit(store, entry).await;
    }
}

/// The body is parsed here, not by an extractor: a body that does not parse is answered and
/// audited with one fixed text, so nothing from it (an older joiner's body holds the invite
/// secret) reaches the answer, the audit log or a log line.
async fn pair(State(store): State<Store>, body: Bytes) -> Response {
    let req: PairRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => {
            let e = another_core::Error::Invalid("invalid pairing request".into());
            record_unauthenticated(&store, None, "POST /fed/pair", "denied", e.to_string()).await;
            return error(StatusCode::BAD_REQUEST, &e);
        }
    };
    let node = req.node.node_id.clone();
    let s = store.clone();
    let (outcome, cause) = tokio::task::spawn_blocking(move || s.accept_pairing_with_cause(&req))
        .await
        .unwrap_or_else(|e| {
            (
                Err(another_core::Error::Invalid(format!("task failed: {e}"))),
                None,
            )
        });
    match outcome {
        Ok(resp) => {
            record(&store, Some(node), "POST /fed/pair", "ok", "paired".into()).await;
            axum::Json(resp).into_response()
        }
        Err(e) => {
            // The caller gets one text for every failure of the invite check; which one it
            // was is recorded here only.
            let detail = cause.map_or_else(|| e.to_string(), |c| c.detail());
            record_unauthenticated(&store, Some(node), "POST /fed/pair", "denied", detail).await;
            error(status_for(&e), &e)
        }
    }
}

/// Verify the signature headers; returns the calling node id and the text of the signature
/// header that verified.
async fn authenticate(
    store: &Store,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(String, String), Box<Response>> {
    let get = |h: &str| {
        headers
            .get(h)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let (Some(node), Some(time), Some(sig)) = (get(H_NODE), get(H_TIME), get(H_SIG)) else {
        let e = another_core::Error::Forbidden("missing node signature headers".into());
        return Err(Box::new(error(StatusCode::UNAUTHORIZED, &e)));
    };
    let time: i64 = time.parse().unwrap_or_default();
    let path = uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    let (s, m, b, n, g) = (
        store.clone(),
        method.to_string(),
        body.to_vec(),
        node.clone(),
        sig.clone(),
    );
    match blocking(move || s.verify_peer_request(&n, &m, &path, time, &b, &g)).await {
        Ok(_) => Ok((node, sig)),
        Err(e) => {
            record_unauthenticated(
                store,
                Some(node),
                &format!("{method} {}", uri.path()),
                "denied",
                e.to_string(),
            )
            .await;
            Err(Box::new(error(StatusCode::UNAUTHORIZED, &e)))
        }
    }
}

/// Refuse a push before its body is read when the node header names no paired, unrevoked
/// spoke, so a caller with no pairing makes the hub buffer nothing. The answer is the one
/// [`authenticate`] gives such a caller after the body. The time header is not judged
/// here: a caller can set it freely, so refusing early on it would stop nobody, and a
/// paired spoke with a wrong clock would lose the answer that tells it so. The signature
/// is checked as before, once the body is read.
async fn known_sender(
    State(store): State<Store>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let Some(node) = req
        .headers()
        .get(H_NODE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
    else {
        let e = another_core::Error::Forbidden("missing node signature headers".into());
        return error(StatusCode::UNAUTHORIZED, &e);
    };
    let (s, n) = (store.clone(), node.clone());
    match blocking(move || s.is_paired_spoke(&n)).await {
        Ok(true) => next.run(req).await,
        Ok(false) => {
            let e = another_core::Error::Forbidden("request not signed by a paired node".into());
            let operation = format!("{} {}", req.method(), req.uri().path());
            record_unauthenticated(&store, Some(node), &operation, "denied", e.to_string()).await;
            error(StatusCode::UNAUTHORIZED, &e)
        }
        Err(e) => error(status_for(&e), &e),
    }
}

/// A nonce as a spoke writes it: 32 lowercase hex characters.
fn is_nonce(text: &str) -> bool {
    text.len() == 32 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The nonce of an authenticated push or pull. A request without one is answered `426`
/// and one with a malformed one `400`, each audited, before anything is stored or read
/// from the log: this hub has no unsigned answer for these routes, and a node that sends
/// no nonce would not check a signed one.
async fn nonce_of(
    store: &Store,
    node: &str,
    operation: &str,
    headers: &HeaderMap,
) -> Result<String, Box<Response>> {
    let Some(value) = headers.get(H_NONCE) else {
        let e = another_core::Error::Invalid(NO_NONCE.into());
        let detail = NO_NONCE_AUDIT.to_string();
        record(store, Some(node.to_string()), operation, "denied", detail).await;
        let mut refusal = error(StatusCode::UPGRADE_REQUIRED, &e);
        refusal
            .headers_mut()
            .insert(header::UPGRADE, HeaderValue::from_static(SIGNED_SYNC));
        return Err(Box::new(refusal));
    };
    match value.to_str() {
        Ok(nonce) if is_nonce(nonce) => Ok(nonce.to_string()),
        _ => {
            let e = another_core::Error::Invalid(BAD_NONCE.into());
            record(
                store,
                Some(node.to_string()),
                operation,
                "denied",
                e.to_string(),
            )
            .await;
            Err(Box::new(error(StatusCode::BAD_REQUEST, &e)))
        }
    }
}

async fn push(
    State(store): State<Store>,
    axum::Extension(exchange): axum::Extension<Exchange>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (node, signature) = match authenticate(&store, &method, &uri, &headers, &body).await {
        Ok(n) => n,
        Err(r) => return *r,
    };
    let nonce = match exchange {
        Exchange::Signed => match nonce_of(&store, &node, "POST /fed/push", &headers).await {
            Ok(nonce) => Some(nonce),
            Err(r) => return *r,
        },
        #[cfg(any(test, feature = "test-support"))]
        Exchange::Unsigned => None,
    };
    let req: PushRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            let e = another_core::Error::Invalid(format!("invalid push body: {e}"));
            return error(StatusCode::BAD_REQUEST, &e);
        }
    };
    let (s, n) = (store.clone(), node.clone());
    match blocking(move || s.ingest(&n, &req.events, &[])).await {
        Ok(report) => {
            let detail = format!(
                "accepted {}, duplicates {}, rejected {}",
                report.accepted,
                report.duplicates,
                report.rejected.len()
            );
            record(&store, Some(node.clone()), "POST /fed/push", "ok", detail).await;
            answer(&store, node, nonce, signature, &report).await
        }
        Err(e) => error(status_for(&e), &e),
    }
}

/// The `200` answer to a push or a pull of `node`: `answer` serialized once, those bytes
/// signed with this hub's key for the request (its nonce and the text of its signature
/// header), and the same bytes sent. The spoke's contact time is written once the answer
/// is built; if that write fails the answer is still sent, since it is correct and the
/// spoke must not repeat an exchange that succeeded. Without a nonce, which only the
/// test-only unsigned exchange allows, the answer is what an older build sent.
async fn answer<T: Serialize>(
    store: &Store,
    node: String,
    nonce: Option<String>,
    request_signature: String,
    answer: &T,
) -> Response {
    let Some(nonce) = nonce else {
        return axum::Json(answer).into_response();
    };
    let body = match serde_json::to_vec(answer) {
        Ok(body) => body,
        Err(e) => {
            let e = another_core::Error::from(e);
            return error(status_for(&e), &e);
        }
    };
    let signature = store.sign_answer(&node, &nonce, &request_signature, &body);
    let response = (
        [
            (
                header::CONTENT_TYPE.as_str(),
                "application/json".to_string(),
            ),
            (H_SIG, signature),
        ],
        body,
    )
        .into_response();
    let (s, n) = (store.clone(), node.clone());
    if let Err(e) = blocking(move || s.record_peer_contact(&n)).await {
        tracing::warn!(node = %node, error = %e, "could not record the spoke's sync time");
    }
    response
}

async fn pull(
    State(store): State<Store>,
    axum::Extension(limits): axum::Extension<Limits>,
    axum::Extension(exchange): axum::Extension<Exchange>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let (node, signature) = match authenticate(&store, &method, &uri, &headers, b"").await {
        Ok(n) => n,
        Err(r) => return *r,
    };
    let nonce = match exchange {
        Exchange::Signed => match nonce_of(&store, &node, "GET /fed/pull", &headers).await {
            Ok(nonce) => Some(nonce),
            Err(r) => return *r,
        },
        #[cfg(any(test, feature = "test-support"))]
        Exchange::Unsigned => None,
    };
    let mut after = 0i64;
    let mut limit = BATCH;
    let mut anchors = PullAnchors::default();
    for pair in uri.query().unwrap_or_default().split('&') {
        match pair.split_once('=') {
            Some(("after", v)) => after = v.parse().unwrap_or(0),
            Some(("limit", v)) => limit = v.parse().unwrap_or(BATCH).clamp(1, 2000),
            Some(("after_id", v)) if is_plain_id(v) => anchors.received = Some(v.to_string()),
            Some(("after_id_seq", v)) => anchors.received_seq = v.parse().ok(),
            Some(("pushed_id", v)) if is_plain_id(v) => anchors.pushed = Some(v.to_string()),
            _ => {}
        }
    }
    let (s, n) = (store.clone(), node.clone());
    let budget = limits.pull_budget;
    // The markers for the events this page steps past are written before it is answered.
    match blocking(move || s.pull_answer(&n, after, &anchors, limit, budget)).await {
        Ok(resp) => {
            let detail = format!("after {after}: {} event(s)", resp.events.len());
            record(&store, Some(node.clone()), "GET /fed/pull", "ok", detail).await;
            answer(&store, node, nonce, signature, &resp).await
        }
        Err(e) => error(status_for(&e), &e),
    }
}

// ---------------------------------------------------------------------------
// Spoke client

fn agent_for(url: &str) -> ureq::Agent {
    let direct = plain_http_loopback(url) == Some(true);
    ureq::Agent::new_with_config(crate::http_client::HttpClient::Federation { direct }.config())
}

fn plain_http_loopback(url: &str) -> Option<bool> {
    let uri = url.parse::<ureq::http::Uri>().ok()?;
    uri.scheme_str()
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("http"))
        .then(|| {
            uri.host()
                .is_some_and(|host| is_loopback_host(&host.to_ascii_lowercase()))
        })
}

#[cfg(test)]
mod redirect_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    #[test]
    fn hub_client_disables_redirects() {
        assert_eq!(
            agent_for("http://127.0.0.1:7466").config().max_redirects(),
            0
        );
    }

    #[test]
    fn hub_redirect_error_handles_missing_or_blank_location() {
        let mut headers = HeaderMap::new();
        let expected = "hub answered 302 redirect without a usable Location header; use the final hub URL in the invite";
        for location in [None, Some(""), Some("  ")] {
            if let Some(location) = location {
                headers.insert(header::LOCATION, HeaderValue::from_static(location));
            }
            let err = refuse_redirect(StatusCode::FOUND, &headers).unwrap_err();
            assert_eq!(err.to_string(), expected);
        }
    }

    #[test]
    fn hub_redirect_is_not_followed() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let destination = TcpListener::bind("127.0.0.1:0").unwrap();
        destination.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let target = destination.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut request = String::new();
            while !request.ends_with("\r\n\r\n") {
                assert!(request.len() < 8192);
                assert!(reader.read_line(&mut request).unwrap() > 0);
            }
            write!(stream, "HTTP/1.1 302 Found\r\nLocation: http://{target}/stolen\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let result = agent_for(&url).get(&format!("{url}/fed/pull")).call();
        server.join().unwrap();
        assert!(
            matches!(destination.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "redirect destination received a connection"
        );
        assert_eq!(result.unwrap().status(), 302);
    }
}

/// A sync request or answer that was over a size limit of the other side or of this one.
/// The caller sends or asks for less and tries again.
#[derive(Debug)]
enum OverSize {
    /// The hub answered 413: the request body was over its limit.
    Request(String),
    /// An I/O error while a request with a body was being sent. A hub that refuses a body
    /// by its declared length answers 413 without reading it and closes, and from here
    /// that can show as a failed connection instead of the answer.
    Dropped(String),
    /// The answer was over the limit this node reads.
    Response(String),
}

impl std::fmt::Display for OverSize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (OverSize::Request(text) | OverSize::Dropped(text) | OverSize::Response(text)) = self;
        f.write_str(text)
    }
}

impl std::error::Error for OverSize {}

/// The body of an answer, of at most `limit` bytes, after content decoding.
fn read_body(
    resp: &mut ureq::http::Response<ureq::Body>,
    what: &str,
    limit: u64,
) -> Result<Vec<u8>> {
    match resp.body_mut().with_config().limit(limit).read_to_vec() {
        Ok(body) => Ok(body),
        Err(ureq::Error::BodyExceedsLimit(_)) => Err(OverSize::Response(format!(
            "the {what} response is larger than the {limit} bytes this node reads"
        ))
        .into()),
        Err(e) => Err(e).with_context(|| format!("reading {what} response")),
    }
}

/// The failure for an answer that is not 2xx. Its body is not signed and is quoted only:
/// the `error.message` in it, or the body itself, as one line of at most `max_chars`
/// characters.
fn failure(status: StatusCode, body: &[u8], what: &str, max_chars: usize) -> anyhow::Error {
    let text = String::from_utf8_lossy(body);
    let msg = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| text.into_owned());
    let msg = if max_chars == usize::MAX {
        msg
    } else {
        another_core::display_line(&msg, max_chars)
    };
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        return OverSize::Request(format!("{what} failed ({status}): {msg}")).into();
    }
    anyhow!("{what} failed ({status}): {msg}")
}

fn refuse_redirect(status: StatusCode, headers: &HeaderMap) -> Result<()> {
    if status.is_redirection() {
        let location = headers
            .get(header::LOCATION)
            .map(|value| {
                another_core::display_line(
                    &String::from_utf8_lossy(value.as_bytes()),
                    ERROR_BODY_CHARS,
                )
            })
            .filter(|location| !location.trim().is_empty());
        let code = status.as_u16();
        let target = match location {
            Some(location) => format!("to {location}"),
            None => "without a usable Location header".to_string(),
        };
        bail!("hub answered {code} redirect {target}; use the final hub URL in the invite");
    }
    Ok(())
}

/// Read a pairing answer of at most `limit` bytes. Its signature is inside the body and is
/// checked by [`Store::complete_join_reporting`].
fn read<T: serde::de::DeserializeOwned>(
    mut resp: ureq::http::Response<ureq::Body>,
    what: &str,
    limit: u64,
) -> Result<T> {
    let status = resp.status();
    refuse_redirect(status, resp.headers())?;
    let body = read_body(&mut resp, what, limit)?;
    if !status.is_success() {
        return Err(failure(status, &body, what, ERROR_BODY_CHARS));
    }
    serde_json::from_slice(&body).with_context(|| format!("decoding {what} response"))
}

/// A push or pull that was sent, with what its answer is checked against.
struct Sent {
    resp: ureq::http::Response<ureq::Body>,
    /// The nonce sent with the request; `None` only for the test-only unsigned exchange.
    nonce: Option<String>,
    /// The text of the request's signature header.
    signature: String,
}

fn signed(
    store: &Store,
    to: &Dial<'_>,
    method: &str,
    path_and_query: &str,
    body: Option<Vec<u8>>,
) -> Result<Sent> {
    let base = to.url;
    let time = store.now();
    let bytes = body.clone().unwrap_or_default();
    let signature = store.sign_request(method, path_and_query, time, &bytes);
    // A new nonce for every request, a retry included. It is not part of the request
    // signature: the hub signs it into its answer.
    let nonce = match to.exchange {
        Exchange::Signed => Some(another_core::crypto::request_nonce()?),
        #[cfg(any(test, feature = "test-support"))]
        Exchange::Unsigned => None,
    };
    let url = format!("{}{path_and_query}", base.trim_end_matches('/'));
    let node = store.node_id().to_string();
    let sent_body = body.is_some();
    let resp = match body {
        Some(b) => {
            let mut request = to
                .agent
                .post(&url)
                .header(H_NODE, &node)
                .header(H_TIME, time.to_string())
                .header(H_SIG, &signature)
                .header("content-type", "application/json");
            if let Some(nonce) = &nonce {
                request = request.header(H_NONCE, nonce);
            }
            request.send(&b[..])
        }
        None => {
            let mut request = to
                .agent
                .get(&url)
                .header(H_NODE, &node)
                .header(H_TIME, time.to_string())
                .header(H_SIG, &signature);
            if let Some(nonce) = &nonce {
                request = request.header(H_NONCE, nonce);
            }
            request.call()
        }
    };
    let resp = resp.map_err(|e| {
        let text = format!("cannot reach the hub at {base}: {e}");
        match &e {
            // Which error the closed connection shows as depends on the system and on
            // timing (broken pipe, reset, and on macOS also "invalid argument"), so every
            // I/O error of a request with a body counts, except a refused connection,
            // where nothing was sent.
            ureq::Error::Io(io)
                if sent_body && io.kind() != std::io::ErrorKind::ConnectionRefused =>
            {
                anyhow::Error::from(OverSize::Dropped(text))
            }
            _ => anyhow!(text),
        }
    })?;
    Ok(Sent {
        resp,
        nonce,
        signature,
    })
}

/// Refuse an answer: write the audit row, and give the error the sync fails with. Nothing
/// of the answer has been used when this is called.
fn refuse(store: &Store, to: &Dial<'_>, what: &str, detail: &str, text: String) -> anyhow::Error {
    audit_refusal(store, to, what, detail.to_string());
    another_core::Error::Forbidden(text).into()
}

/// One `denied` row in this node's audit log for an answer of its hub that it refused.
fn audit_refusal(store: &Store, to: &Dial<'_>, what: &str, detail: String) {
    let operation = if what == "push" {
        "sync POST /fed/push"
    } else {
        "sync GET /fed/pull"
    };
    let row = AuditRecord {
        token_id: None,
        agent: Some(format!("node:{}", to.hub)),
        operation: operation.to_string(),
        outcome: "denied".to_string(),
        detail: Some(detail),
        remote: None,
    };
    if let Err(e) = store.record_audit(row) {
        tracing::warn!(error = %e, "could not write the audit row for a refused answer");
    }
}

/// Read the answer to a push or a pull of at most `limit` bytes, and check it before one
/// field of it is read: a 2xx answer must carry the hub's signature over this node's
/// request (its nonce and its signature) and the body as received. An answer that is not
/// 2xx is not signed and is only ever a failure. On a refusal nothing of the answer is
/// used, and an audit row is written.
fn read_answer<T: serde::de::DeserializeOwned>(
    store: &Store,
    to: &Dial<'_>,
    sent: Sent,
    what: &str,
    limit: u64,
) -> Result<T> {
    let Sent {
        mut resp,
        nonce,
        signature,
    } = sent;
    let status = resp.status();
    refuse_redirect(status, resp.headers())?;
    let answer_signature = resp
        .headers()
        .get(H_SIG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = read_body(&mut resp, what, limit)?;
    let Some(nonce) = nonce else {
        // As a build before signed answers: the error quoted whole, the answer unchecked.
        if !status.is_success() {
            return Err(failure(status, &body, what, usize::MAX));
        }
        return serde_json::from_slice(&body).with_context(|| format!("decoding {what} response"));
    };
    let address = shown_address(to.url);
    if status == StatusCode::UPGRADE_REQUIRED {
        // The hub's own text for this status tells an older node to update, which this
        // node is not: it sent a nonce, so the header was lost on the way or the refusal
        // is not the hub's.
        return Err(refuse(
            store,
            to,
            what,
            "refused as carrying no nonce, although one was sent",
            format!(
                "the hub at {address} refused the {what} as carrying no nonce, but this node \
                 sent one; nothing was applied. Something between this node and the hub \
                 removed the header, or the refusal is not the hub's; if a proxy in front of \
                 the hub demands TLS, the hub's address may need to be https://"
            ),
        ));
    }
    if !status.is_success() {
        return Err(failure(status, &body, what, ERROR_BODY_CHARS));
    }
    let Some(answer_signature) = answer_signature else {
        return Err(refuse(
            store,
            to,
            what,
            "unsigned answer",
            format!(
                "the hub at {address} answered the {what} without a signature; refused, and \
                 nothing from that answer was applied. Either the hub runs an older build \
                 (update the hub and its spokes together), or something between this node \
                 and the hub answered for it or removed the signature"
            ),
        ));
    };
    match store.verify_hub_answer(to.hub, &nonce, &signature, &body, &answer_signature) {
        Ok(()) => {}
        Err(another_core::Error::Forbidden(_)) => {
            return Err(refuse(
                store,
                to,
                what,
                "answer signature is not the hub's for this request",
                format!(
                    "the hub at {address} answered the {what} with a signature that is not \
                     the hub's (key {}) for this request and this body; refused, and nothing \
                     from that answer was applied. Something between this node and the hub \
                     changed the answer or replayed another one; a proxy that rewrites \
                     answer bodies has the same effect",
                    to.hub_key
                ),
            ))
        }
        Err(e) => return Err(e.into()),
    }
    serde_json::from_slice(&body).with_context(|| format!("decoding {what} response"))
}

/// Whether a hub address is plain `http://` to a host that is not this machine: anyone on
/// the network path can then read what a sync carries. Loopback is the parsed host being
/// `127.0.0.1`, `localhost` (case insensitive) or `[::1]`.
pub fn plain_http_off_loopback(url: &str) -> bool {
    plain_http_loopback(url) == Some(false)
}

/// A hub address as a warning or a refusal shows it: one line, cut.
pub fn shown_address(url: &str) -> String {
    another_core::display_line(url, ADDRESS_CHARS)
}

/// What `another pair invite` says after the code when the address given to it is plain
/// `http://` off loopback. `url` is the `--url` argument; the address is shown as the
/// invite holds it.
pub fn invite_warning(url: &str) -> Option<String> {
    let address = url.trim_end_matches('/');
    plain_http_off_loopback(address).then(|| {
        format!(
            "warning: {} is plain http://. Anyone on the network path between a spoke and \
             this hub can read mail and the feed in transit. Give spokes an https:// address \
             unless the link is a private network",
            shown_address(address)
        )
    })
}

/// What `another pair join` says after it joined a hub at a plain `http://` address off
/// loopback.
pub fn join_warning(hub: &Peer) -> Option<&'static str> {
    hub.url
        .as_deref()
        .is_some_and(plain_http_off_loopback)
        .then_some(
            "warning: this hub is reached over plain http://. Anyone on the network path can \
             read this node's mail and feed in transit; they cannot change them, because the \
             hub's answers are signed. Ask for an https:// address unless the link is a \
             private network",
        )
}

/// This node's hub, when it was joined by a build before invites were signed: its stored
/// address is one nothing checked. `None` when there is no such hub, and when the question
/// cannot be answered, since what is said about it is advice.
pub fn hub_with_unchecked_address(store: &Store) -> Option<Peer> {
    let hub = store.hub_joined_before_signed_invites().ok().flatten()?;
    store.peers().ok()?.into_iter().find(|p| p.node_id == hub)
}

/// The address in an invite is where this node will send its pairing request and then every
/// sync, and it is stored. It must be an `http://` or `https://` URL with a host; anything
/// else is refused before anything is sent. This checks the form only, by the rule the hub
/// applied before it signed the address into the invite.
fn hub_url_is_http(url: &str) -> another_core::Result<()> {
    if another_core::hub_url_is_http(url) {
        Ok(())
    } else {
        Err(another_core::Error::Invalid(
            "this invite's hub address is not an http:// or https:// URL with a host; not joined"
                .into(),
        ))
    }
}

/// Spoke: redeem an invite code with the hub and record it as this node's hub.
pub fn join(store: &Store, code: &str) -> Result<Peer> {
    join_reporting(store, code).map(|(peer, _)| peer)
}

/// [`join`], and beside the peer the name this node had stored for the hub when the join
/// lifted this node's own revoke of it.
pub fn join_reporting(store: &Store, code: &str) -> Result<(Peer, Option<String>)> {
    let (invite, req) = store.prepare_join(code)?;
    hub_url_is_http(&invite.hub_url)?;
    let agent = agent_for(&invite.hub_url);
    let resp = agent
        .post(&format!("{}/fed/pair", invite.hub_url))
        .header("content-type", "application/json")
        .send(serde_json::to_vec(&req)?.as_slice())
        .map_err(|e| anyhow!("cannot reach the hub at {}: {e}", invite.hub_url))?;
    let paired: PairResponse = read(resp, "pairing", REPLY_LIMIT_BYTES)?;
    Ok(store.complete_join_reporting(&invite, &paired)?)
}

#[derive(Debug, Default, Serialize)]
pub struct SyncReport {
    pub hub: String,
    pub pushed: usize,
    pub push_duplicates: usize,
    pub push_rejected: Vec<Rejected>,
    pub pulled: usize,
    pub applied: usize,
    pub pull_duplicates: usize,
    pub pull_rejected: Vec<Rejected>,
    /// Pulled events that were stored but not applied here: a peer's hold whose pattern is
    /// over the limit or does not compile.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub not_applied: Vec<Rejected>,
    /// The hub's log no longer matched this spoke's cursors (it was restored from an older
    /// copy, or replaced), so this sync started over from zero in both directions.
    pub hub_reset: bool,
    /// The unacknowledged skips of this node's log and of its hub's log: events too large
    /// to send, reported after every sync until acknowledged. The oldest five at most, so a
    /// peer that writes many markers cannot make this report large; `skipped_total` counts
    /// them all. `another needs --json` lists up to its row limit, and
    /// `another activity --kinds sync` lists them, at most 500 per call; page with
    /// `--since <time or event id>`.
    pub skipped: Vec<Skip>,
    /// How many unacknowledged skips there are, the ones in `skipped` included.
    pub skipped_total: usize,
}

/// An event id that can go in a query string as is. Ids this software makes are ULIDs;
/// anything else is not sent as an anchor, and the hub then checks the cursor alone.
fn is_plain_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Spoke: push events this node authored, then pull everyone else's, until caught up. If the
/// hub reports that its log went backwards (`reset`), rewind both cursors and run once more,
/// so events the hub lost are pushed again and its new events are not skipped.
pub fn sync(store: &Store) -> Result<SyncReport> {
    sync_with(store, &Limits::CURRENT)
}

/// [`sync`] with the size bounds given.
pub fn sync_with(store: &Store, limits: &Limits) -> Result<SyncReport> {
    sync_as(store, limits, Exchange::Signed)
}

/// [`sync_with`] as a spoke of the build `exchange` names.
pub fn sync_as(store: &Store, limits: &Limits, exchange: Exchange) -> Result<SyncReport> {
    let (hub, url) = store
        .hub()?
        .ok_or_else(|| anyhow!("this node has no hub; pair it with `another pair join <code>` (hubs do not sync, spokes dial them)"))?;
    let agent = agent_for(&url);
    let mut report = SyncReport {
        hub: url.clone(),
        ..Default::default()
    };
    let mut last = PassLast::default();
    let hub_key = store
        .peers()?
        .into_iter()
        .find(|p| p.node_id == hub)
        .map(|p| p.fingerprint)
        .unwrap_or_default();
    let to = Dial {
        agent: &agent,
        hub: &hub,
        hub_key: &hub_key,
        url: &url,
        limits,
        exchange,
    };
    if !sync_pass(store, &to, &mut report, &mut last)? {
        report.hub_reset = true;
        store.reset_sync_cursors(&hub)?;
        last = PassLast::default();
        // From zero no anchor is sent, so the hub cannot report reset again.
        if !sync_pass(store, &to, &mut report, &mut last)? {
            return Err(anyhow!(
                "the hub at {url} reported its log reset twice in one sync; try again"
            ));
        }
        // Each page and batch already committed its cursor with its anchor; this records the
        // resync, and clears an anchor when the restored hub sent or accepted nothing.
        let received = last.received.as_ref().map(|(id, seq)| (id.as_str(), *seq));
        store.record_resync(&hub, received, last.pushed.as_deref())?;
    }
    // The one write of the hub's sync time: only a sync that ran to its end counts.
    store.record_sync_complete(&hub)?;

    let mut skipped = store.unacknowledged_skips()?;
    report.skipped_total = skipped.len();
    skipped.truncate(another_core::SYNC_SKIPS_SHOWN);
    report.skipped = skipped;
    Ok(report)
}

/// The last event the hub sent in a pass, with its hub `seq`, and the last of this node's
/// events it accepted.
#[derive(Default)]
struct PassLast {
    received: Option<(String, i64)>,
    pushed: Option<String>,
}

/// The hub a sync dials and the size bounds it runs with.
struct Dial<'a> {
    agent: &'a ureq::Agent,
    hub: &'a str,
    /// The fingerprint of the key pinned for the hub, for the text of a refusal.
    hub_key: &'a str,
    url: &'a str,
    limits: &'a Limits,
    exchange: Exchange,
}

/// One push and pull. Returns false, having stopped early, when the hub reports `reset`.
fn sync_pass(
    store: &Store,
    to: &Dial<'_>,
    report: &mut SyncReport,
    last: &mut PassLast,
) -> Result<bool> {
    let Dial {
        hub, url, limits, ..
    } = *to;
    // The anchors are read before this pass pushes anything: a push can only add events to
    // the hub, never bring back the ones a restore lost.
    let mut anchors = Some(store.pull_anchors()?);
    let (push_start, _) = store.push_cursor(hub)?;
    // Push. A batch is bounded by bytes as well as by count. A hub on a build before D99
    // reads a smaller body than this node's budget and answers 413, or closes the
    // connection on the unread body: the batch is then cut to half as many events, down to
    // one, for the rest of this pass. From 500 that is at most eight cuts, so a network
    // that is simply failing costs at most nine failed requests before its error is given.
    // An own event that does not fit a push even alone is stepped past: its marker is
    // written with the cursor, and the next turn of this loop pushes the marker.
    let mut batch = BATCH;
    loop {
        let (cursor, _) = store.push_cursor(hub)?;
        let (events, high, skipped) =
            store.outbound_events_within(cursor, batch, limits.push_budget)?;
        if high == cursor {
            break;
        }
        let mut accepted = None;
        if !events.is_empty() {
            let ids: Vec<String> = events.iter().map(|e| e.event_id.clone()).collect();
            let kind = events[0].kind.clone();
            let body = serde_json::to_vec(&PushRequest { events })?;
            let size = body.len();
            let answer = signed(store, to, "POST", "/fed/push", Some(body)).and_then(|sent| {
                read_answer::<IngestReport>(store, to, sent, "push", REPLY_LIMIT_BYTES)
            });
            let r = match answer {
                Ok(r) => r,
                Err(e) => match e.downcast_ref() {
                    Some(OverSize::Request(_) | OverSize::Dropped(_)) if ids.len() > 1 => {
                        batch = ids.len() as u32 / 2;
                        continue;
                    }
                    Some(OverSize::Request(_)) => bail!(
                        "the hub at {url} refused event {} ({kind}) as too large: it alone \
                         makes a push of {size} bytes, over what the hub reads in one request; \
                         retrying will not help; the hub runs a build with a smaller limit \
                         than this node's, so update the hub ({e})",
                        ids[0]
                    ),
                    Some(OverSize::Dropped(_)) => bail!(
                        "the connection to the hub at {url} failed while event {} ({kind}) was \
                         sent alone in a push of {size} bytes; if this happens every time, the \
                         hub is refusing it as too large and retrying will not help: the hub \
                         runs a build with a smaller limit than this node's, so update the \
                         hub ({e})",
                        ids[0]
                    ),
                    _ => return Err(e),
                },
            };
            report.pushed += r.accepted;
            report.push_duplicates += r.duplicates;
            // The hub holds every event it did not reject: accepted now, or before.
            accepted = ids
                .into_iter()
                .rev()
                .find(|id| !r.rejected.iter().any(|x| &x.event_id == id));
            report.push_rejected.extend(r.rejected);
        }
        store.advance_push_past(hub, high, accepted.as_deref(), &skipped)?;
        if accepted.is_some() {
            last.pushed = accepted;
        }
    }
    // Pull. The first page carries the anchors. The received anchor goes only with a pull
    // cursor, and the pushed one only with a push cursor: from zero there is nothing to lose.
    // A hub on a build before D99 bounds a page by count only, so its answer can be over
    // what this node reads: the page is then asked for again with half the count, down to
    // one event, for the rest of this pass. The anchors go with every try of the first page.
    let mut limit = BATCH;
    loop {
        let (_, cursor) = store.push_cursor(hub)?;
        let mut path = format!("/fed/pull?after={cursor}&limit={limit}");
        if let Some(a) = &anchors {
            if let Some(id) = a
                .received
                .as_ref()
                .filter(|id| cursor > 0 && is_plain_id(id))
            {
                path.push_str(&format!("&after_id={id}"));
                if let Some(seq) = a.received_seq {
                    path.push_str(&format!("&after_id_seq={seq}"));
                }
            }
            if let Some(id) = a
                .pushed
                .as_ref()
                .filter(|id| push_start > 0 && is_plain_id(id))
            {
                path.push_str(&format!("&pushed_id={id}"));
            }
        }
        let sent = signed(store, to, "GET", &path, None)?;
        let answer = read_answer(store, to, sent, "pull", limits.pull_body as u64);
        let page: PullResponse = match answer {
            Ok(page) => page,
            Err(e) if !matches!(e.downcast_ref(), Some(OverSize::Response(_))) => return Err(e),
            Err(_) if limit > 1 => {
                limit /= 2;
                continue;
            }
            Err(e) => bail!(
                "the next event the hub at {url} holds for this node, after its sequence \
                 number {cursor}, is by itself larger than this node reads in one answer; \
                 retrying will not help; the hub runs an older build or holds an event over \
                 the size limit; a hub on this build or later steps past such an event and \
                 reports it, so update the hub ({e})"
            ),
        };
        anchors = None;
        if page.reset {
            return Ok(false);
        }
        report.pulled += page.events.len();
        let r = match store.ingest(hub, &page.events, &page.nodes) {
            Ok(r) => r,
            Err(e) => {
                // A listed key that differs from the pinned one: the refusal rolled back
                // with the page, so its audit row is written here, after it.
                if let Some(conflict) = store.listed_key_conflict(&page.nodes)? {
                    if conflict.refusal().to_string() == e.to_string() {
                        audit_refusal(store, to, "pull", conflict.detail());
                    }
                }
                return Err(e.into());
            }
        };
        report.applied += r.accepted;
        report.pull_duplicates += r.duplicates;
        report.pull_rejected.extend(r.rejected);
        report.not_applied.extend(r.not_applied);
        // The cursor and the anchor (the last event the hub sent, stored or not) commit
        // together, so neither can move without the other (REVIEW-R1 and R2 P2).
        let page_last = page.events.last().map(|e| (e.event_id.as_str(), e.seq));
        store.advance_pull(hub, page.cursor, page_last)?;
        if let Some((id, seq)) = page_last {
            last.received = Some((id.to_string(), seq));
        }
        if !page.more || page.cursor == cursor {
            break;
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    #[test]
    fn signed_request_uses_manual_store_clock() {
        let (clock, _) = another_core::Clock::manual(1_234_567_890_000);
        let store = Store::open_in_memory(clock).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&stream);
            let mut headers = Vec::new();
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                headers.push(line.trim().to_ascii_lowercase());
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
            headers
        });
        let agent = agent_for(&url);
        let to = Dial {
            agent: &agent,
            hub: "test-hub",
            hub_key: "test-key",
            url: &url,
            limits: &Limits::CURRENT,
            exchange: Exchange::Unsigned,
        };
        let sent = signed(&store, &to, "GET", "/fed/pull", None);
        let headers = server.join().unwrap();
        let sent = sent.unwrap();
        assert!(headers.contains(&format!("{H_TIME}: {}", store.now())));
        assert_eq!(
            sent.signature,
            store.sign_request("GET", "/fed/pull", store.now(), &[])
        );
    }
}
