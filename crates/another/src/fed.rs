//! Federation transport: hub endpoints (`/fed/pair`, `/fed/push`, `/fed/pull`) and the spoke
//! client that dials out to them (`another pair join`, `another sync`).
//!
//! Node-to-node requests are signed with the sending node's key over method, path, time and
//! body; the hub checks them against the key it pinned at pairing.

use std::time::Duration;

use another_core::{
    AuditRecord, IngestReport, PairRequest, PairResponse, Peer, PullAnchors, PullResponse,
    PushRequest, Rejected, Skip, Store,
};
use anyhow::{anyhow, bail, Context, Result};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::json;

const H_NODE: &str = "x-another-node";
const H_TIME: &str = "x-another-time";
const H_SIG: &str = "x-another-signature";
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

/// Routes a hub serves when started with `another serve --federation`.
pub fn router(store: Store) -> axum::Router {
    router_with(store, Limits::CURRENT)
}

/// [`router`] with the size bounds given. Each route that reads a body states its limit;
/// `/fed/pull` is a GET and reads none.
pub fn router_with(store: Store, limits: Limits) -> axum::Router {
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
    crate::remote::audit(
        store,
        AuditRecord {
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
        },
    )
    .await;
}

/// The body is parsed here, not by an extractor: a body that does not parse is answered and
/// audited with one fixed text, so nothing from it (an older joiner's body holds the invite
/// secret) reaches the answer, the audit log or a log line.
async fn pair(State(store): State<Store>, body: Bytes) -> Response {
    let req: PairRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => {
            let e = another_core::Error::Invalid("invalid pairing request".into());
            record(&store, None, "POST /fed/pair", "denied", e.to_string()).await;
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
            record(&store, Some(node), "POST /fed/pair", "denied", detail).await;
            error(status_for(&e), &e)
        }
    }
}

/// Verify the signature headers; returns the calling node id.
async fn authenticate(
    store: &Store,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<String, Box<Response>> {
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
    let (s, m, b, n) = (
        store.clone(),
        method.to_string(),
        body.to_vec(),
        node.clone(),
    );
    match blocking(move || s.verify_peer_request(&n, &m, &path, time, &b, &sig)).await {
        Ok(_) => Ok(node),
        Err(e) => {
            record(
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
            record(&store, Some(node), &operation, "denied", e.to_string()).await;
            error(StatusCode::UNAUTHORIZED, &e)
        }
        Err(e) => error(status_for(&e), &e),
    }
}

async fn push(
    State(store): State<Store>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let node = match authenticate(&store, &method, &uri, &headers, &body).await {
        Ok(n) => n,
        Err(r) => return *r,
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
            record(&store, Some(node), "POST /fed/push", "ok", detail).await;
            axum::Json(report).into_response()
        }
        Err(e) => error(status_for(&e), &e),
    }
}

async fn pull(
    State(store): State<Store>,
    axum::Extension(limits): axum::Extension<Limits>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let node = match authenticate(&store, &method, &uri, &headers, b"").await {
        Ok(n) => n,
        Err(r) => return *r,
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
            record(&store, Some(node), "GET /fed/pull", "ok", detail).await;
            axum::Json(resp).into_response()
        }
        Err(e) => error(status_for(&e), &e),
    }
}

// ---------------------------------------------------------------------------
// Spoke client

fn agent_for(url: &str) -> ureq::Agent {
    let loopback = ["http://127.0.0.1", "http://localhost", "http://[::1]"]
        .iter()
        .any(|p| url.starts_with(p));
    let mut config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)));
    if loopback {
        config = config.proxy(None);
    }
    ureq::Agent::new_with_config(config.build())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
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

/// Read an answer of at most `limit` bytes.
fn read<T: serde::de::DeserializeOwned>(
    mut resp: ureq::http::Response<ureq::Body>,
    what: &str,
    limit: u64,
) -> Result<T> {
    let status = resp.status();
    let text = match resp.body_mut().with_config().limit(limit).read_to_string() {
        Ok(text) => text,
        Err(ureq::Error::BodyExceedsLimit(_)) => {
            return Err(OverSize::Response(format!(
                "the {what} response is larger than the {limit} bytes this node reads"
            ))
            .into())
        }
        Err(e) => return Err(e).with_context(|| format!("reading {what} response")),
    };
    if !status.is_success() {
        let msg = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
            .unwrap_or(text);
        if status == StatusCode::PAYLOAD_TOO_LARGE {
            return Err(OverSize::Request(format!("{what} failed ({status}): {msg}")).into());
        }
        bail!("{what} failed ({status}): {msg}");
    }
    serde_json::from_str(&text).with_context(|| format!("decoding {what} response"))
}

fn signed(
    store: &Store,
    agent: &ureq::Agent,
    base: &str,
    method: &str,
    path_and_query: &str,
    body: Option<Vec<u8>>,
) -> Result<ureq::http::Response<ureq::Body>> {
    let time = now_ms();
    let bytes = body.clone().unwrap_or_default();
    let sig = store.sign_request(method, path_and_query, time, &bytes);
    let url = format!("{}{path_and_query}", base.trim_end_matches('/'));
    let node = store.node_id().to_string();
    let sent_body = body.is_some();
    let resp = match body {
        Some(b) => agent
            .post(&url)
            .header(H_NODE, &node)
            .header(H_TIME, time.to_string())
            .header(H_SIG, &sig)
            .header("content-type", "application/json")
            .send(&b[..]),
        None => agent
            .get(&url)
            .header(H_NODE, &node)
            .header(H_TIME, time.to_string())
            .header(H_SIG, &sig)
            .call(),
    };
    resp.map_err(|e| {
        let text = format!("cannot reach the hub at {base}: {e}");
        match &e {
            // Which error the closed connection shows as depends on the system and on
            // timing (broken pipe, reset, and on macOS also "invalid argument"), so every
            // I/O error of a request with a body counts, except a refused connection,
            // where nothing was sent.
            ureq::Error::Io(io)
                if sent_body && io.kind() != std::io::ErrorKind::ConnectionRefused =>
            {
                OverSize::Dropped(text).into()
            }
            _ => anyhow!(text),
        }
    })
}

/// The address in an invite is where this node will send its pairing request and then every
/// sync, and it is stored. It must be an `http://` or `https://` URL with a host; anything
/// else is refused before anything is sent. This checks the form only: an invite is not
/// signed, so the address is whatever the code's author put there.
fn hub_url_is_http(url: &str) -> another_core::Result<()> {
    let ok = url.parse::<ureq::http::Uri>().is_ok_and(|uri| {
        matches!(uri.scheme_str(), Some("http" | "https"))
            && uri.host().is_some_and(|h| !h.is_empty())
    });
    if ok {
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
    let (hub, url) = store
        .hub()?
        .ok_or_else(|| anyhow!("this node has no hub; pair it with `another pair join <code>` (hubs do not sync, spokes dial them)"))?;
    let agent = agent_for(&url);
    let mut report = SyncReport {
        hub: url.clone(),
        ..Default::default()
    };
    let mut last = PassLast::default();
    let to = Dial {
        agent: &agent,
        hub: &hub,
        url: &url,
        limits,
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
    url: &'a str,
    limits: &'a Limits,
}

/// One push and pull. Returns false, having stopped early, when the hub reports `reset`.
fn sync_pass(
    store: &Store,
    to: &Dial<'_>,
    report: &mut SyncReport,
    last: &mut PassLast,
) -> Result<bool> {
    let Dial {
        agent,
        hub,
        url,
        limits,
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
            let answer = signed(store, agent, url, "POST", "/fed/push", Some(body))
                .and_then(|resp| read::<IngestReport>(resp, "push", REPLY_LIMIT_BYTES));
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
        let resp = signed(store, agent, url, "GET", &path, None)?;
        let page: PullResponse = match read(resp, "pull", limits.pull_body as u64) {
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
        let r = store.ingest(hub, &page.events, &page.nodes)?;
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
