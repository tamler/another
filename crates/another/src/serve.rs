//! `another serve`: the local daemon. Serves MCP over stdio or loopback streamable HTTP and
//! runs the sweeper that expires lapsed holds and marks overdue requests stale.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

use another_core::Store;
use anyhow::{bail, Context, Result};
use axum::response::IntoResponse;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::ServiceExt;
use tokio_util::sync::CancellationToken;

use crate::mcp::AnotherMcp;

pub const DEFAULT_BIND: &str = "127.0.0.1:7466";

/// HTTP serving options.
#[derive(Debug, Clone, Default)]
pub struct HttpOptions {
    /// Require bearer tokens on `/mcp` and `/activity`.
    pub auth: bool,
    /// Explicitly allow a non-loopback bind (only together with `auth`).
    pub allow_remote_bind: bool,
    /// Extra `Host` header values to accept (tunnel hostnames).
    /// An entry with no port matches only a `Host` header with no port.
    pub allowed_hosts: Vec<String>,
    /// Public origin for OAuth (`--public-url`). Bearer tokens work without it.
    pub oauth: Option<crate::oauth_http::OAuthConfig>,
    /// TCP port this process bound. Loopback `Host` values are accepted only with it.
    pub bound_port: u16,
    /// Serve the federation hub endpoints (`/fed/*`), authenticated by node signatures.
    pub federation: bool,
    /// `Some(true)` is `--dashboard`, `Some(false)` is `--no-dashboard`, `None` is the default.
    pub dashboard_flag: Option<bool>,
    /// Effective dashboard mode. [`serve_http`] sets this from the bind and [`dashboard_flag`].
    pub dashboard: crate::dashboard::DashboardMode,
    /// Address actually bound, so the dashboard can accept that exact `Host`.
    /// `127.0.0.1`, `localhost`, and `[::1]` are always allowed as well.
    pub dashboard_bound: Option<SocketAddr>,
    /// Run the push waker this often (wake idle Herdr agents with unannounced mail).
    /// `None` turns it off; only the local loopback daemon should run one.
    pub push_every: Option<Duration>,
}

/// Resolve a bind address and refuse anything that is not loopback.
pub fn local_bind(addr: &str) -> Result<SocketAddr> {
    check_bind(addr, &HttpOptions::default())
}

/// Resolve a bind address. Loopback is always fine; anything else needs both `--auth` and
/// `--allow-remote-bind`.
pub fn check_bind(addr: &str, opts: &HttpOptions) -> Result<SocketAddr> {
    let resolved: Vec<SocketAddr> = addr
        .to_socket_addrs()
        .with_context(|| format!("invalid bind address {addr:?} (expected host:port)"))?
        .collect();
    let Some(first) = resolved.first().copied() else {
        bail!("bind address {addr:?} did not resolve");
    };
    if resolved.iter().all(|a| a.ip().is_loopback()) {
        return Ok(first);
    }
    if opts.auth && opts.allow_remote_bind {
        return Ok(first);
    }
    bail!(
        "refusing to bind {addr}: another serves on loopback (127.0.0.1 / ::1 / localhost) unless \
         both --auth and --allow-remote-bind are given; prefer a tunnel to a loopback --auth server"
    )
}

/// Periodically expire holds and stale requests until cancelled.
pub fn spawn_sweeper(store: Store, every: Duration, ct: CancellationToken) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        loop {
            tokio::select! {
                _ = ct.cancelled() => break,
                _ = tick.tick() => {
                    let store = store.clone();
                    match tokio::task::spawn_blocking(move || store.sweep()).await {
                        Ok(Ok(r)) if r.expired_holds + r.stale_requests > 0 => tracing::info!(
                            expired_holds = r.expired_holds,
                            stale_requests = r.stale_requests,
                            "sweep"
                        ),
                        Ok(Ok(_)) => {}
                        Ok(Err(e)) => tracing::warn!(error = %e, "sweep failed"),
                        Err(e) => tracing::warn!(error = %e, "sweep task failed"),
                    }
                }
            }
        }
    });
}

/// How often the waker also drops wake targets that can no longer be reached.
const PRUNE_EVERY: Duration = Duration::from_secs(600);

/// Run the push waker (see [`crate::wake::Waker`]) and periodic target pruning until
/// cancelled. Each pass runs on the blocking pool, since it touches SQLite and may start
/// a `herdr` process.
pub fn spawn_waker(store: Store, every: Duration, ct: CancellationToken) {
    let first = crate::wake::Waker::new();
    // On shutdown, tell a pass in progress to stop its adapter commands now; the blocking
    // pass then returns within moments instead of waiting out a stuck adapter.
    let cancel = first.cancel.clone();
    let stop = ct.clone();
    tokio::spawn(async move {
        stop.cancelled().await;
        cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    tokio::spawn(async move {
        let mut waker = Some(first);
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut prune_tick = tokio::time::interval(PRUNE_EVERY);
        loop {
            tokio::select! {
                _ = ct.cancelled() => break,
                _ = tick.tick() => {
                    let Some(mut w) = waker.take() else { break };
                    let store = store.clone();
                    match tokio::task::spawn_blocking(move || {
                        let out = w.tick(&store);
                        (w, out)
                    })
                    .await
                    {
                        Ok((w, out)) => {
                            waker = Some(w);
                            match out {
                                Ok(events) => {
                                    for e in events {
                                        tracing::info!(
                                            agent = %e.agent,
                                            target = %e.target,
                                            action = %e.action,
                                            detail = %e.detail,
                                            "push"
                                        );
                                    }
                                }
                                Err(e) => tracing::warn!(error = %format!("{e:#}"), "push pass failed"),
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "push task failed; push delivery stopped");
                            break;
                        }
                    }
                }
                _ = prune_tick.tick() => {
                    let store = store.clone();
                    match tokio::task::spawn_blocking(move || crate::wake::prune(&store, false)).await {
                        Ok(Ok(rows)) => {
                            for r in rows.iter().filter(|r| r.pruned) {
                                tracing::info!(agent = %r.agent, target = %r.target, reason = %r.reason, "wake target pruned");
                            }
                        }
                        Ok(Err(e)) => tracing::warn!(error = %format!("{e:#}"), "prune failed"),
                        Err(e) => tracing::warn!(error = %e, "prune task failed"),
                    }
                }
            }
        }
    });
}

/// Resolves on Ctrl-C, or on SIGTERM (what launchd and systemd send to stop a service).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

pub async fn serve_stdio(store: Store, sweep_every: Duration) -> Result<()> {
    let ct = CancellationToken::new();
    spawn_sweeper(store.clone(), sweep_every, ct.clone());
    tracing::info!(node_id = store.node_id(), "serving MCP on stdio");
    let service = AnotherMcp::for_stdio(store)
        .serve(rmcp::transport::stdio())
        .await
        .context("starting stdio MCP server")?;
    service.waiting().await?;
    ct.cancel();
    Ok(())
}

/// Build the HTTP app: MCP at `/mcp`, the activity feed at `GET /activity`, and
/// `GET /health`. With `auth`, `/mcp` and `/activity` require a bearer token.
pub fn http_router(store: Store, ct: CancellationToken, opts: &HttpOptions) -> axum::Router {
    let factory_store = store.clone();
    let mut config =
        StreamableHttpServerConfig::default().with_cancellation_token(ct.child_token());
    if opts.auth && (!opts.allowed_hosts.is_empty() || opts.oauth.is_some()) {
        let mut hosts = config.allowed_hosts.clone();
        hosts.extend(opts.allowed_hosts.iter().cloned());
        if let Some(oauth) = &opts.oauth {
            hosts.push(oauth.hostname.clone());
        }
        config = config.with_allowed_hosts(hosts);
    }
    let mcp: StreamableHttpService<AnotherMcp, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(AnotherMcp::new(factory_store.clone())),
        Default::default(),
        config,
    );
    let protected = axum::Router::new()
        .nest_service("/mcp", mcp)
        .route("/activity", axum::routing::get(crate::remote::activity))
        .with_state(store.clone());
    let protected = if opts.auth {
        protected.layer(axum::middleware::from_fn_with_state(
            store.clone(),
            crate::remote::require_token,
        ))
    } else {
        protected
    };
    let protected = if opts.federation {
        protected.merge(crate::fed::router(store.clone()))
    } else {
        protected
    };
    let node_id = store.node_id().to_string();
    let auth = opts.auth;
    let dashboard = opts.dashboard.as_str().to_string();
    let mut app = protected.route(
        "/health",
        axum::routing::get(move || {
            let node_id = node_id.clone();
            let dashboard = dashboard.clone();
            async move {
                axum::Json(serde_json::json!({
                    "ok": true,
                    "node_id": node_id,
                    "auth": auth,
                    "dashboard": dashboard,
                }))
            }
        }),
    );
    if opts.auth {
        let ctx = crate::oauth_http::AuthContext {
            oauth: opts.oauth.clone(),
        };
        if opts.oauth.is_some() {
            app = app.merge(crate::oauth_http::router().with_state(store.clone()));
        }
        app = app.layer(axum::Extension(ctx));
    }
    // Mounted after the auth layer, same as /health, so a token is never required.
    // Actions and message bodies are still withheld unless the mode is local.
    // The edge guard below still applies. The dashboard router adds its own
    // loopback Host and bound-port Origin checks on top of that.
    if opts.dashboard.enabled() {
        app = app.merge(crate::dashboard::router(
            store,
            opts.dashboard,
            opts.dashboard_bound,
        ));
    }
    // DNS-rebinding checks run for unauthenticated loopback too, not only `--auth`.
    let guard = crate::oauth_http::EdgeGuard::from_http(
        opts.bound_port,
        &opts.allowed_hosts,
        opts.oauth.as_ref(),
    );
    app.layer(axum::middleware::from_fn(move |req, next| {
        let guard = guard.clone();
        async move { enforce_edge(guard, req, next).await }
    }))
}

/// `Host` on every route, and `Origin` on state-changing requests that send one.
/// Federation authenticates with node signatures and keeps its own checks.
async fn enforce_edge(
    guard: crate::oauth_http::EdgeGuard,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let path = req.uri().path();
    if path.starts_with("/fed") {
        return next.run(req).await;
    }
    let host_ok = req
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|h| guard.hosts.allows(h));
    if !host_ok {
        return (
            axum::http::StatusCode::FORBIDDEN,
            "host is not allowed; pass the tunnel hostname with --allowed-host or --public-url",
        )
            .into_response();
    }
    if changes_state(req.method()) {
        if let Some(origin) = req
            .headers()
            .get(axum::http::header::ORIGIN)
            .and_then(|v| v.to_str().ok())
        {
            if !guard.origins.allows(origin) {
                return (axum::http::StatusCode::FORBIDDEN, "origin is not allowed")
                    .into_response();
            }
        }
    }
    if let Some(limits) = &guard.limits {
        if let Some(class) = limit_class(path) {
            let ip = req
                .extensions()
                .get::<axum::extract::ConnectInfo<SocketAddr>>()
                .map(|peer| peer.0.ip())
                .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
            if let Err(retry_after) = limits.allow(class, ip) {
                return too_many(retry_after);
            }
        }
    }
    next.run(req).await
}

fn changes_state(method: &axum::http::Method) -> bool {
    matches!(
        *method,
        axum::http::Method::POST
            | axum::http::Method::PUT
            | axum::http::Method::PATCH
            | axum::http::Method::DELETE
    )
}

fn limit_class(path: &str) -> Option<&'static str> {
    match path {
        "/oauth/token" => Some("token"),
        "/oauth/register" => Some("register"),
        "/authorize" => Some("authorize"),
        _ => None,
    }
}

fn too_many(retry_after: u64) -> axum::response::Response {
    let mut res = (
        axum::http::StatusCode::TOO_MANY_REQUESTS,
        axum::Json(serde_json::json!({
            "error": "slow_down",
            "error_description": "too many requests",
        })),
    )
        .into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(&retry_after.to_string()) {
        res.headers_mut()
            .insert(axum::http::header::RETRY_AFTER, value);
    }
    res.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    res
}

pub async fn serve_http(
    store: Store,
    bind: &str,
    opts: HttpOptions,
    sweep_every: Duration,
) -> Result<()> {
    let addr = check_bind(bind, &opts)?;
    let mut opts = opts;
    opts.dashboard = crate::dashboard::resolve_dashboard(
        addr.ip().is_loopback(),
        opts.auth,
        opts.dashboard_flag,
    )?;
    let ct = CancellationToken::new();
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    // Background work starts only once the port is ours, so a second daemon that fails to
    // bind never sweeps or wakes alongside the first.
    spawn_sweeper(store.clone(), sweep_every, ct.clone());
    if let Some(every) = opts.push_every {
        spawn_waker(store.clone(), every, ct.clone());
        eprintln!(
            "another: push delivery on: idle Herdr agents with new mail are woken (every {}s)",
            every.as_secs().max(1)
        );
    }
    let local = listener.local_addr()?;
    opts.bound_port = local.port();
    opts.dashboard_bound = Some(local);
    if !local.ip().is_loopback() {
        // Accept requests addressed to the bound IP itself, on this port only.
        opts.allowed_hosts.push(socket_host(local));
    }
    let auth_note = if opts.oauth.is_some() {
        " (OAuth or bearer token)"
    } else if opts.auth {
        " (bearer token required)"
    } else {
        ""
    };
    eprintln!(
        "another: MCP (streamable HTTP) on http://{local}/mcp{auth_note}, node {}",
        store.node_id()
    );
    if let Some(oauth) = &opts.oauth {
        eprintln!("another: hosted apps connect to {}", oauth.resource);
        eprintln!(
            "another: approve a client with `another oauth approve <code> --agent NAME --project '/path/*'`"
        );
    } else if opts.auth {
        eprintln!(
            "another: OAuth is off. Pass --public-url (the tunnel origin) so ChatGPT and Claude can connect. Bearer tokens still work."
        );
    }
    if opts.dashboard.enabled() {
        eprintln!(
            "another: dashboard at http://{local}/ui ({})",
            opts.dashboard.as_str()
        );
    }
    if opts.dashboard == crate::dashboard::DashboardMode::Redacted {
        eprintln!(
            "another: warning: --dashboard on an --auth server is read-only and omits message \
             bodies. A tunnel pointed at this port exposes that view. Actions stay off."
        );
    }
    let app = http_router(store, ct.clone(), &opts);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        ct.cancel();
    })
    .await?;
    Ok(())
}

fn socket_host(addr: SocketAddr) -> String {
    match addr.ip() {
        IpAddr::V4(ip) => format!("{ip}:{}", addr.port()),
        IpAddr::V6(ip) => format!("[{ip}]:{}", addr.port()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_binds_are_allowed() {
        assert!(local_bind("127.0.0.1:0").is_ok());
        assert!(local_bind("[::1]:0").is_ok());
        assert!(local_bind("localhost:0").is_ok());
        assert!(local_bind("0.0.0.0:7466").is_err());
        assert!(local_bind("[::]:7466").is_err());
        assert!(local_bind("192.168.1.10:7466").is_err());
        assert!(local_bind("not an address").is_err());
    }

    #[test]
    fn remote_binds_need_auth_and_explicit_flag() {
        let auth_only = HttpOptions {
            auth: true,
            ..Default::default()
        };
        assert!(check_bind("0.0.0.0:0", &auth_only).is_err());
        let flag_only = HttpOptions {
            allow_remote_bind: true,
            ..Default::default()
        };
        assert!(check_bind("0.0.0.0:0", &flag_only).is_err());
        let both = HttpOptions {
            auth: true,
            allow_remote_bind: true,
            ..Default::default()
        };
        assert!(check_bind("0.0.0.0:0", &both).is_ok());
        assert!(check_bind("127.0.0.1:0", &auth_only).is_ok());
    }
}
