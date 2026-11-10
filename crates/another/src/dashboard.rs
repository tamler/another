//! Local web dashboard. Static files are embedded; nothing is fetched at runtime.
//!
//! The dashboard is loopback-only. An `--auth` server does not mount it unless
//! `--dashboard` is passed, and then it is read-only with message bodies omitted,
//! because a tunnel can deliver traffic that looks like loopback. Actions
//! (release a hold, expire a request) exist only on an unauthenticated loopback
//! server and require an explicit confirm.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

use another_core::{ActivityKind, DashActivityQuery, DashboardView, Since, Store};
use axum::body::Bytes;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio_stream::wrappers::ReceiverStream;

const HTML: &str = include_str!("dashboard/assets/index.html");
const CSS: &str = include_str!("dashboard/assets/app.css");
const JS: &str = include_str!("dashboard/assets/app.js");

/// Whether, and how, this process serves the dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DashboardMode {
    #[default]
    Off,
    /// Unauthenticated loopback: bodies and confirmed actions.
    Local,
    /// Explicitly enabled on an authenticated loopback server. Read-only, no bodies.
    Redacted,
}

impl DashboardMode {
    pub fn enabled(self) -> bool {
        self != Self::Off
    }

    pub fn bodies(self) -> bool {
        self == Self::Local
    }

    pub fn actions(self) -> bool {
        self == Self::Local
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Local => "local",
            Self::Redacted => "redacted",
        }
    }

    fn view(self) -> DashboardView {
        if self.bodies() {
            DashboardView::LOCAL
        } else {
            DashboardView::REDACTED
        }
    }
}

/// Decide if a server should mount the dashboard.
///
/// `explicit` is `Some(true)` for `--dashboard`, `Some(false)` for `--no-dashboard`,
/// and `None` for the default: on for loopback without auth, off otherwise.
pub fn resolve_dashboard(
    loopback: bool,
    auth: bool,
    explicit: Option<bool>,
) -> anyhow::Result<DashboardMode> {
    match (explicit, loopback, auth) {
        (Some(false), _, _) => Ok(DashboardMode::Off),
        (Some(true), false, _) => anyhow::bail!(
            "refusing --dashboard on a non-loopback bind: the dashboard is loopback-only \
             and is never served from a remote address"
        ),
        (Some(true), true, true) => Ok(DashboardMode::Redacted),
        (Some(true), true, false) => Ok(DashboardMode::Local),
        (None, true, false) => Ok(DashboardMode::Local),
        (None, _, _) => Ok(DashboardMode::Off),
    }
}

#[derive(Clone)]
struct DashState {
    store: Store,
    mode: DashboardMode,
    /// The address this process bound, when known. Its exact `Host` form is allowed
    /// in addition to `127.0.0.1`, `localhost`, and `[::1]`.
    bound: Option<SocketAddr>,
}

pub fn router(store: Store, mode: DashboardMode, bound: Option<SocketAddr>) -> Router {
    let state = DashState { store, mode, bound };
    Router::new()
        .route("/ui", get(index))
        .route("/ui/", get(index))
        .route("/ui/app.css", get(css))
        .route("/ui/app.js", get(js))
        .route("/api/dash", get(snapshot))
        .route("/api/dash/activity", get(activity))
        .route("/api/dash/revision", get(revision))
        .route("/api/dash/stream", get(stream))
        .route("/api/dash/holds/{id}/release", post(release_hold))
        .route("/api/dash/requests/{id}/expire", post(expire_request))
        .with_state(state.clone())
        .layer(axum::middleware::from_fn_with_state(
            state,
            require_loopback_host,
        ))
}

/// DNS rebinding sends a non-loopback `Host` to a loopback socket. Refuse it
/// before any dashboard byte, including the page, the read APIs, and SSE.
async fn require_loopback_host(
    State(state): State<DashState>,
    request: Request,
    next: Next,
) -> Response {
    let host = request_host(&request);
    if loopback_host(host.as_deref().unwrap_or(""), state.bound.as_ref()) {
        next.run(request).await
    } else {
        forbidden("dashboard requests must use a loopback Host")
    }
}

fn request_host(request: &Request) -> Option<String> {
    if let Some(value) = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
    {
        return Some(value.to_string());
    }
    request.uri().authority().map(|a| a.as_str().to_string())
}

/// `127.0.0.1`, `localhost`, and `[::1]`, with or without a port, or the exact bound address.
fn loopback_host(host: &str, bound: Option<&SocketAddr>) -> bool {
    let host = host.trim();
    if host.is_empty() || host.len() > 255 {
        return false;
    }
    if let Some(addr) = bound {
        if host.eq_ignore_ascii_case(&addr.to_string()) {
            return true;
        }
    }
    let Some((name, port)) = split_host(host) else {
        return false;
    };
    if let Some(port) = port {
        if !valid_port(port) {
            return false;
        }
    }
    name == "127.0.0.1" || name.eq_ignore_ascii_case("localhost") || name == "::1"
}

fn split_host(host: &str) -> Option<(&str, Option<&str>)> {
    if let Some(rest) = host.strip_prefix('[') {
        let (name, after) = rest.split_once(']')?;
        if name.is_empty() || name.contains(['/', ' ', '@']) {
            return None;
        }
        let port = if after.is_empty() {
            None
        } else {
            Some(after.strip_prefix(':')?.trim())
        };
        if port.is_some_and(str::is_empty) {
            return None;
        }
        return Some((name, port));
    }
    if let Some((name, port)) = host.rsplit_once(':') {
        if name.is_empty() || name.contains(':') || port.is_empty() {
            return None;
        }
        return Some((name, Some(port)));
    }
    Some((host, None))
}

fn valid_port(port: &str) -> bool {
    matches!(port.parse::<u16>(), Ok(n) if n > 0)
}

/// `http://` plus an allowed loopback host. No suffix match, no other scheme, no path.
fn loopback_origin(origin: &str, bound: Option<&SocketAddr>) -> bool {
    let origin = origin.trim();
    let Some((scheme, host)) = origin.split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("http") {
        return false;
    }
    if host.is_empty() || host.contains(['/', '?', '#', '@', '\\', ' ']) {
        return false;
    }
    loopback_host(host, bound)
}

/// Dashboard-only server used by `another dashboard`. Always local, always loopback.
pub async fn serve_standalone(
    store: Store,
    bind: &str,
    open: bool,
    json_out: bool,
) -> anyhow::Result<()> {
    let addr = crate::serve::local_bind(bind)?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| anyhow::anyhow!("binding {addr}: {e}"))?;
    let local = listener.local_addr()?;
    let url = format!("http://{local}/ui");
    if json_out {
        println!("{}", json!({ "url": url, "mode": "local" }));
        let _ = std::io::Write::flush(&mut std::io::stdout());
    } else {
        eprintln!("another: dashboard at {url}");
        eprintln!(
            "another: loopback only. Message bodies are included. Actions ask for a confirm."
        );
    }
    if open {
        open_browser(&url);
    }
    let app = router(store, DashboardMode::Local, Some(local)).route(
        "/health",
        get(|| async { Json(json!({ "ok": true, "dashboard": "local" })) }),
    );
    axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

pub fn open_browser(url: &str) {
    let mut cmd = match std::env::consts::OS {
        "macos" => {
            let mut c = std::process::Command::new("open");
            c.arg(url);
            c
        }
        "windows" => {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", "start", "", url]);
            c
        }
        _ => {
            let mut c = std::process::Command::new("xdg-open");
            c.arg(url);
            c
        }
    };
    let _ = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

async fn index() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'self'; script-src 'self'; img-src 'self' data:; \
                 connect-src 'self'; font-src 'self'; frame-ancestors 'none'; base-uri 'none'; \
                 form-action 'self'",
            ),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        HTML,
    )
        .into_response()
}

async fn css() -> Response {
    asset("text/css; charset=utf-8", CSS)
}

async fn js() -> Response {
    asset("text/javascript; charset=utf-8", JS)
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-cache"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        body,
    )
        .into_response()
}

async fn snapshot(State(state): State<DashState>) -> Response {
    let store = state.store.clone();
    let view = state.mode.view();
    match block(move || store.dashboard_snapshot(view)).await {
        Ok(snap) => Json(snap).into_response(),
        Err(resp) => *resp,
    }
}

#[derive(Debug, Deserialize)]
struct ActivityParams {
    q: Option<String>,
    agent: Option<String>,
    project: Option<String>,
    kind: Option<String>,
    since: Option<String>,
    until: Option<String>,
    before: Option<String>,
    limit: Option<u32>,
}

async fn activity(
    State(state): State<DashState>,
    Query(params): Query<ActivityParams>,
) -> Response {
    let store = state.store.clone();
    let include_bodies = state.mode.bodies();
    let now = store.now();
    let since_ms = match params
        .since
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        None | Some("all") => None,
        Some(raw) => match Since::parse(raw, now) {
            Ok(Since::Time(t)) => Some(t),
            Ok(Since::After(_)) => {
                return invalid("since must be a duration like 1d or an RFC 3339 time");
            }
            Err(e) => return invalid(&e.to_string()),
        },
    };
    let until_ms = match params
        .until
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        None => None,
        Some(raw) => match Since::parse(raw, now) {
            Ok(Since::Time(t)) => Some(t),
            _ => return invalid("until must be an RFC 3339 time"),
        },
    };
    let mut kinds = Vec::new();
    if let Some(raw) = params.kind.as_deref() {
        for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match part.parse::<ActivityKind>() {
                Ok(k) => kinds.push(k),
                Err(e) => return invalid(&e.to_string()),
            }
        }
    }
    let query = DashActivityQuery {
        text: params.q,
        agent: params.agent,
        project: params.project,
        kinds,
        since_ms,
        until_ms,
        before_id: params.before,
        limit: params.limit.unwrap_or(80),
        include_bodies,
    };
    match block(move || store.dashboard_activity(query)).await {
        Ok(page) => Json(page).into_response(),
        Err(resp) => *resp,
    }
}

async fn revision(State(state): State<DashState>) -> Response {
    let store = state.store.clone();
    match block(move || store.dashboard_seq()).await {
        Ok(seq) => Json(json!({ "seq": seq })).into_response(),
        Err(resp) => *resp,
    }
}

async fn stream(
    State(state): State<DashState>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let store = state.store.clone();
    tokio::spawn(async move {
        let mut last = i64::MIN;
        loop {
            let probe = store.clone();
            let seq = tokio::task::spawn_blocking(move || probe.dashboard_seq()).await;
            let Ok(Ok(seq)) = seq else { break };
            if seq != last {
                last = seq;
                let ev = Event::default()
                    .event("revision")
                    .data(json!({ "seq": seq }).to_string());
                if tx.send(Ok(ev)).await.is_err() {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
    Sse::new(ReceiverStream::new(rx)).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("tick"),
    )
}

#[derive(Debug, Deserialize)]
struct ConfirmBody {
    confirm: Option<bool>,
}

async fn release_hold(
    State(state): State<DashState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(resp) = ensure_actions(&state) {
        return *resp;
    }
    if let Err(resp) = confirmed(&state, &headers, &body) {
        return *resp;
    }
    let store = state.store.clone();
    let released = id.clone();
    match block(move || store.release_hold_from_dashboard(&id)).await {
        Ok(()) => Json(json!({ "ok": true, "released": released })).into_response(),
        Err(resp) => *resp,
    }
}

async fn expire_request(
    State(state): State<DashState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(resp) = ensure_actions(&state) {
        return *resp;
    }
    if let Err(resp) = confirmed(&state, &headers, &body) {
        return *resp;
    }
    let store = state.store.clone();
    match block(move || store.expire_request_from_dashboard(&id)).await {
        Ok(request) => Json(request).into_response(),
        Err(resp) => *resp,
    }
}

fn ensure_actions(state: &DashState) -> Result<(), Box<Response>> {
    if state.mode.actions() {
        Ok(())
    } else {
        Err(Box::new(forbidden(
            "dashboard actions are only available on an unauthenticated loopback server",
        )))
    }
}

fn confirmed(state: &DashState, headers: &HeaderMap, body: &Bytes) -> Result<(), Box<Response>> {
    if headers
        .get("x-another-confirm")
        .and_then(|v| v.to_str().ok())
        != Some("1")
    {
        return Err(Box::new(invalid(
            "set header X-Another-Confirm: 1 and send {\"confirm\": true}",
        )));
    }
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !loopback_origin(origin, state.bound.as_ref()) {
        return Err(Box::new(forbidden(
            "dashboard actions require an exact loopback Origin",
        )));
    }
    let parsed: ConfirmBody = match serde_json::from_slice(body) {
        Ok(body) => body,
        Err(_) => return Err(Box::new(invalid("send a JSON body {\"confirm\": true}"))),
    };
    if parsed.confirm != Some(true) {
        return Err(Box::new(invalid("confirm must be true")));
    }
    Ok(())
}

fn invalid(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": { "code": "invalid", "message": message } })),
    )
        .into_response()
}

fn forbidden(message: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "error": { "code": "forbidden", "message": message } })),
    )
        .into_response()
}

fn core_err(err: another_core::Error) -> Response {
    let status = match err.code() {
        "not_found" => StatusCode::NOT_FOUND,
        "conflict" => StatusCode::CONFLICT,
        "forbidden" => StatusCode::FORBIDDEN,
        "invalid" | "ambiguous" => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(json!({ "error": { "code": err.code(), "message": err.to_string() } })),
    )
        .into_response()
}

async fn block<T: Send + 'static>(
    f: impl FnOnce() -> another_core::Result<T> + Send + 'static,
) -> Result<T, Box<Response>> {
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => Err(Box::new(core_err(err))),
        Err(_) => Err(Box::new(StatusCode::INTERNAL_SERVER_ERROR.into_response())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_stays_off_remote_and_auth_unless_asked() {
        assert_eq!(
            resolve_dashboard(true, false, None).unwrap(),
            DashboardMode::Local
        );
        assert_eq!(
            resolve_dashboard(true, true, None).unwrap(),
            DashboardMode::Off
        );
        assert_eq!(
            resolve_dashboard(false, true, None).unwrap(),
            DashboardMode::Off
        );
        assert_eq!(
            resolve_dashboard(true, false, Some(false)).unwrap(),
            DashboardMode::Off
        );
        assert_eq!(
            resolve_dashboard(true, true, Some(true)).unwrap(),
            DashboardMode::Redacted
        );
        assert!(resolve_dashboard(false, true, Some(true)).is_err());
        assert!(resolve_dashboard(false, false, Some(true)).is_err());
    }

    #[test]
    fn host_and_origin_allow_only_exact_loopback() {
        let bound: SocketAddr = "127.0.0.2:7468".parse().unwrap();
        assert!(loopback_host("127.0.0.1", None));
        assert!(loopback_host("127.0.0.1:7466", None));
        assert!(loopback_host("localhost", None));
        assert!(loopback_host("LOCALHOST:80", None));
        assert!(loopback_host("[::1]", None));
        assert!(loopback_host("[::1]:7466", None));
        assert!(loopback_host("127.0.0.2:7468", Some(&bound)));
        assert!(!loopback_host("evil.example", None));
        assert!(!loopback_host("evil.example:80", None));
        assert!(!loopback_host("127.0.0.1.evil.example", None));
        assert!(!loopback_host("127.0.0.1:7466:1", None));
        assert!(!loopback_host("::1", None));
        assert!(!loopback_host("", None));

        assert!(loopback_origin("http://127.0.0.1:7466", None));
        assert!(loopback_origin("http://localhost", None));
        assert!(loopback_origin("http://[::1]:7466", None));
        assert!(!loopback_origin("http://evil.example", None));
        assert!(!loopback_origin("http://127.0.0.1.evil.example", None));
        assert!(!loopback_origin("https://127.0.0.1", None));
        assert!(!loopback_origin("http://127.0.0.1/ui", None));
        assert!(!loopback_origin("", None));
    }
}
