//! HTTP surface of the OAuth 2.1 authorization server.
//!
//! Mounted only on `another serve --auth` when `--public-url` is set. Approval is never
//! done over HTTP: `/authorize` records a waiting request and the browser polls until
//! `another oauth approve` runs on the machine that holds the database.

use std::collections::HashMap;
use std::io::Read;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

use another_core::{
    canonical_resource, error_redirect, redirect_matches, validate_cimd, AuthorizeInput,
    ClientProfile, ClientRegistration, CodeExchange, OAuthFail, RefreshExchange, Store,
};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

/// Where hosted apps reach this node. Built from `--public-url`.
#[derive(Clone)]
pub struct OAuthConfig {
    pub issuer: String,
    pub resource: String,
    pub hostname: String,
    pub loopback: bool,
    cimd: Arc<Mutex<HashMap<String, CachedCimd>>>,
}

struct CachedCimd {
    profile: ClientProfile,
    expires: Instant,
}

/// `Host` values accepted on every HTTP route except `/fed`.
///
/// Loopback names are stored with the bound port. An entry with no port matches
/// only a `Host` header that also has no port.
#[derive(Clone, Debug)]
pub struct HostPolicy {
    allowed: Vec<String>,
}

/// Origins accepted on state-changing requests that send an `Origin` header.
/// Comparison is the whole origin, never a suffix.
#[derive(Clone, Debug)]
pub struct OriginPolicy {
    allowed: Vec<String>,
}

/// Per-address token buckets for the OAuth write endpoints. `None` when OAuth is off.
#[derive(Clone)]
pub struct RateLimiter {
    inner: Arc<Mutex<HashMap<String, Bucket>>>,
}

struct Bucket {
    tokens: u32,
    updated: Instant,
}

/// Host, origin, and (when OAuth is on) rate-limit policy for one server.
#[derive(Clone)]
pub struct EdgeGuard {
    pub hosts: HostPolicy,
    pub origins: OriginPolicy,
    pub limits: Option<RateLimiter>,
}

/// How many requests one client IP may send before `/oauth/register` returns 429.
pub const REGISTER_BURST: u32 = 8;
/// How many requests one client IP may send before `/authorize` returns 429.
pub const AUTHORIZE_BURST: u32 = 12;
/// How many requests one client IP may send before `/oauth/token` returns 429.
pub const TOKEN_BURST: u32 = 20;

const REGISTER_EVERY: Duration = Duration::from_secs(6);
const AUTHORIZE_EVERY: Duration = Duration::from_secs(3);
const TOKEN_EVERY: Duration = Duration::from_secs(2);

/// Shared by the auth middleware and the OAuth routes.
#[derive(Clone)]
pub struct AuthContext {
    pub oauth: Option<OAuthConfig>,
}

impl std::fmt::Debug for OAuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthConfig")
            .field("issuer", &self.issuer)
            .field("resource", &self.resource)
            .field("hostname", &self.hostname)
            .field("loopback", &self.loopback)
            .finish()
    }
}

impl OAuthConfig {
    pub fn metadata_url(&self) -> String {
        format!("{}/.well-known/oauth-protected-resource/mcp", self.issuer)
    }

    /// `Host` header value for this origin. A non-default port is included.
    pub fn host_value(&self) -> String {
        self.issuer
            .split_once("://")
            .map(|(_, rest)| rest.to_string())
            .unwrap_or_else(|| self.hostname.clone())
    }
}

impl HostPolicy {
    /// Loopback names are accepted only on `port` (the port this process bound).
    pub fn for_bind(port: u16, extra: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let mut allowed = vec![
            format!("localhost:{port}"),
            format!("127.0.0.1:{port}"),
            format!("[::1]:{port}"),
        ];
        for raw in extra {
            let raw = raw.into().trim().to_ascii_lowercase();
            if raw.is_empty() || allowed.iter().any(|e| e == &raw) {
                continue;
            }
            allowed.push(raw);
        }
        Self { allowed }
    }

    pub fn allows(&self, host_header: &str) -> bool {
        let Some((host, port)) = split_host_header(host_header) else {
            return false;
        };
        self.allowed.iter().any(|entry| {
            split_host_header(entry).is_some_and(|(allowed_host, allowed_port)| {
                allowed_host == host && allowed_port == port
            })
        })
    }
}

impl OriginPolicy {
    pub fn for_bind(port: u16, extra: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let mut allowed = Vec::new();
        for raw in [
            format!("http://localhost:{port}"),
            format!("http://127.0.0.1:{port}"),
            format!("http://[::1]:{port}"),
        ] {
            if let Some(origin) = canonical_origin(&raw) {
                allowed.push(origin);
            }
        }
        for raw in extra {
            if let Some(origin) = canonical_origin(&raw.into()) {
                if !allowed.iter().any(|e| e == &origin) {
                    allowed.push(origin);
                }
            }
        }
        Self { allowed }
    }

    pub fn allows(&self, origin: &str) -> bool {
        let Some(origin) = canonical_origin(origin) else {
            return false;
        };
        self.allowed.iter().any(|allowed| allowed == &origin)
    }
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Take one token. `Err` is the `Retry-After` delay in seconds.
    pub fn allow(&self, class: &str, ip: IpAddr) -> Result<(), u64> {
        let (capacity, every) = match class {
            "register" => (REGISTER_BURST, REGISTER_EVERY),
            "authorize" => (AUTHORIZE_BURST, AUTHORIZE_EVERY),
            "token" => (TOKEN_BURST, TOKEN_EVERY),
            _ => return Ok(()),
        };
        self.take(&format!("{class}|{ip}"), capacity, every)
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimiter {
    fn take(&self, key: &str, capacity: u32, every: Duration) -> Result<(), u64> {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if map.len() > 4096 {
            map.clear();
        }
        let now = Instant::now();
        let bucket = map.entry(key.to_string()).or_insert(Bucket {
            tokens: capacity,
            updated: now,
        });
        let elapsed = now.saturating_duration_since(bucket.updated);
        let steps = elapsed.as_nanos() / every.as_nanos().max(1);
        if steps > 0 {
            let add = u32::try_from(steps).unwrap_or(u32::MAX);
            bucket.tokens = bucket.tokens.saturating_add(add).min(capacity);
            bucket.updated = now;
        }
        if bucket.tokens == 0 {
            let wait = every.saturating_sub(now.saturating_duration_since(bucket.updated));
            return Err(wait.as_secs().max(1));
        }
        bucket.tokens -= 1;
        Ok(())
    }
}

impl EdgeGuard {
    pub fn from_http(
        bound_port: u16,
        allowed_hosts: &[String],
        oauth: Option<&OAuthConfig>,
    ) -> Self {
        let mut extra = allowed_hosts.to_vec();
        let mut origins = Vec::new();
        if let Some(cfg) = oauth {
            extra.push(cfg.host_value());
            origins.push(cfg.issuer.clone());
        }
        Self {
            hosts: HostPolicy::for_bind(bound_port, extra),
            origins: OriginPolicy::for_bind(bound_port, origins),
            limits: oauth.map(|_| RateLimiter::new()),
        }
    }
}

/// `WWW-Authenticate` for a 401. Points at protected-resource metadata when OAuth is on.
pub fn www_authenticate(oauth: Option<&OAuthConfig>) -> String {
    match oauth {
        Some(cfg) => format!(
            "Bearer resource_metadata=\"{}\", scope=\"another\"",
            cfg.metadata_url()
        ),
        None => "Bearer".into(),
    }
}

/// Parse `--public-url`. `http` is accepted only for a loopback host (local tests).
pub fn parse_public_url(raw: &str) -> anyhow::Result<OAuthConfig> {
    let raw = raw.trim().trim_end_matches('/');
    let (scheme, rest) = raw.split_once("://").ok_or_else(|| {
        anyhow::anyhow!("--public-url must look like https://another.example.com")
    })?;
    let scheme = scheme.to_ascii_lowercase();
    if rest.contains('/') || rest.contains('?') || rest.contains('#') || rest.contains('@') {
        anyhow::bail!("--public-url is an origin with no path, query or userinfo");
    }
    let (host, port) = split_authority(rest)?;
    let loopback = is_loopback_name(&host);
    if scheme == "http" && !loopback {
        anyhow::bail!("--public-url must be https (http is only allowed for loopback tests)");
    }
    if scheme != "http" && scheme != "https" {
        anyhow::bail!("--public-url scheme must be https");
    }
    let port = match (scheme.as_str(), port) {
        ("https", Some(443)) | ("http", Some(80)) => None,
        (_, other) => other,
    };
    let issuer = format!("{scheme}://{}", format_host(&host, port));
    let resource = canonical_resource(&format!("{issuer}/mcp"))
        .map_err(|e| anyhow::anyhow!("--public-url: {e}"))?;
    Ok(OAuthConfig {
        issuer,
        resource,
        hostname: host,
        loopback,
        cimd: Arc::new(Mutex::new(HashMap::new())),
    })
}

fn is_loopback_name(host: &str) -> bool {
    host == "localhost" || host == "127.0.0.1" || host == "::1"
}

fn format_host(host: &str, port: Option<u16>) -> String {
    let mut out = String::new();
    if host.contains(':') {
        out.push('[');
        out.push_str(host);
        out.push(']');
    } else {
        out.push_str(host);
    }
    if let Some(port) = port {
        out.push(':');
        out.push_str(&port.to_string());
    }
    out
}

fn split_authority(authority: &str) -> anyhow::Result<(String, Option<u16>)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .ok_or_else(|| anyhow::anyhow!("invalid host in --public-url"))?;
        if host.is_empty() {
            anyhow::bail!("--public-url is missing a host");
        }
        let port = if after.is_empty() {
            None
        } else {
            Some(parse_port(after.strip_prefix(':').ok_or_else(|| {
                anyhow::anyhow!("invalid host in --public-url")
            })?)?)
        };
        return Ok((host.to_ascii_lowercase(), port));
    }
    if authority.matches(':').count() > 1 {
        anyhow::bail!("invalid host in --public-url; wrap IPv6 addresses in brackets");
    }
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => (h, Some(parse_port(p)?)),
        None => (authority, None),
    };
    if host.is_empty() {
        anyhow::bail!("--public-url is missing a host");
    }
    Ok((host.to_ascii_lowercase(), port))
}

fn parse_port(raw: &str) -> anyhow::Result<u16> {
    let port: u16 = raw
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid port in --public-url"))?;
    if port == 0 {
        anyhow::bail!("invalid port in --public-url");
    }
    Ok(port)
}

fn normalize_host(host: &str) -> String {
    host.trim_matches(['[', ']']).to_ascii_lowercase()
}

fn split_host_header(header: &str) -> Option<(String, Option<u16>)> {
    let header = header.trim();
    if header.is_empty()
        || header.len() > 512
        || header.contains('/')
        || header.contains(' ')
        || header.contains(',')
    {
        return None;
    }
    if let Ok(authority) = axum::http::uri::Authority::try_from(header) {
        let host = normalize_host(authority.host());
        if host.is_empty() {
            return None;
        }
        return Some((host, authority.port_u16()));
    }
    if header.contains(':') {
        return None;
    }
    let host = normalize_host(header);
    if host.is_empty() {
        None
    } else {
        Some((host, None))
    }
}

/// Serialize an origin the way the browser does: lowercase scheme and host,
/// default ports omitted, no userinfo, path, query, or fragment. Returns
/// `None` when the value is not a single origin. Matching is equality of this
/// string, not a suffix check.
fn canonical_origin(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > 512 || raw.eq_ignore_ascii_case("null") {
        return None;
    }
    let (scheme, rest) = raw.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    if rest.contains('@')
        || rest.contains('/')
        || rest.contains('?')
        || rest.contains('#')
        || rest.contains('\\')
        || rest.contains(' ')
    {
        return None;
    }
    let (host, port) = split_authority(rest).ok()?;
    let port = match (scheme.as_str(), port) {
        ("https", Some(443)) | ("http", Some(80)) => None,
        (_, other) => other,
    };
    Some(format!("{scheme}://{}", format_host(&host, port)))
}

pub fn router() -> Router<Store> {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server),
        )
        .route(
            "/.well-known/openid-configuration",
            get(authorization_server),
        )
        .route("/authorize", get(authorize))
        .route("/oauth/register", post(register_client))
        .route("/oauth/token", post(token))
        .route("/oauth/revoke", post(revoke))
        .route("/oauth/wait/{request_id}", get(wait_page))
        .route("/oauth/pending/{request_id}", get(pending))
}

fn cfg(ctx: &AuthContext) -> Option<&OAuthConfig> {
    ctx.oauth.as_ref()
}

fn no_store(res: Response) -> Response {
    let mut res = res;
    res.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    res.headers_mut()
        .insert(header::PRAGMA, header::HeaderValue::from_static("no-cache"));
    res
}

fn json_doc(body: Value) -> Response {
    ([(header::CACHE_CONTROL, "max-age=60")], Json(body)).into_response()
}

async fn protected_resource(Extension(ctx): Extension<AuthContext>) -> Response {
    let Some(cfg) = cfg(&ctx) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    json_doc(json!({
        "resource": cfg.resource,
        "authorization_servers": [cfg.issuer],
        "scopes_supported": ["another"],
        "bearer_methods_supported": ["header"],
        "resource_documentation": "https://github.com/tamler/another/blob/main/docs/REMOTE.md"
    }))
}

async fn authorization_server(Extension(ctx): Extension<AuthContext>) -> Response {
    let Some(cfg) = cfg(&ctx) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let base = &cfg.issuer;
    json_doc(json!({
        "issuer": cfg.issuer,
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/oauth/token"),
        "registration_endpoint": format!("{base}/oauth/register"),
        "revocation_endpoint": format!("{base}/oauth/revoke"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "client_id_metadata_document_supported": true,
        "authorization_response_iss_parameter_supported": true,
        "scopes_supported": ["another", "offline_access"],
        "response_modes_supported": ["query"]
    }))
}

#[derive(Debug, Deserialize)]
struct AuthorizeQuery {
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    resource: Option<String>,
    state: Option<String>,
    scope: Option<String>,
}

fn client_ip(headers: &HeaderMap) -> Option<String> {
    for name in ["cf-connecting-ip", "x-forwarded-for", "x-real-ip"] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
            let ip = v.split(',').next().unwrap_or(v).trim();
            return Some(format!("{name}: {ip}"));
        }
    }
    None
}

fn audit_oauth(
    store: &Store,
    operation: &str,
    outcome: &str,
    detail: &str,
    remote: Option<String>,
) {
    let store = store.clone();
    let operation = operation.to_string();
    let outcome = outcome.to_string();
    let detail = detail.to_string();
    tokio::spawn(async move {
        let _ = tokio::task::spawn_blocking(move || {
            store.record_audit(another_core::AuditRecord {
                operation,
                outcome,
                detail: Some(detail),
                remote,
                ..Default::default()
            })
        })
        .await;
    });
}

fn fail_page(err: &OAuthFail) -> Response {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Another</title>\
         <p>{}</p><p>{}</p>",
        esc(err.code()),
        esc(err.description())
    );
    no_store((StatusCode::BAD_REQUEST, Html(body)).into_response())
}

fn maybe_redirect(
    err: &OAuthFail,
    redirect_uri: Option<&str>,
    state: Option<&str>,
    issuer: &str,
    trusted: bool,
) -> Response {
    if trusted {
        if let Some(uri) = redirect_uri {
            let loc = error_redirect(uri, err.code(), err.description(), state, issuer);
            return no_store(Redirect::to(&loc).into_response());
        }
    }
    fail_page(err)
}

async fn authorize(
    State(store): State<Store>,
    Extension(ctx): Extension<AuthContext>,
    headers: HeaderMap,
    Query(q): Query<AuthorizeQuery>,
) -> Response {
    let Some(cfg) = cfg(&ctx).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let remote = client_ip(&headers);
    let client_id = q.client_id.clone().unwrap_or_default();
    let redirect_uri = q.redirect_uri.clone().unwrap_or_default();
    let redirect_for_error = redirect_uri.clone();
    let state_for_error = q.state.clone();
    let remote_for_error = remote.clone();
    let client = match resolve_client(&store, &cfg, &client_id).await {
        Ok(c) => c,
        Err(e) => {
            audit_oauth(&store, "oauth.authorize", "denied", e.description(), remote);
            return fail_page(&e);
        }
    };
    let trusted = redirect_matches(&client.redirect_uris, &redirect_uri);
    let fail = |e: OAuthFail| {
        audit_oauth(
            &store,
            "oauth.authorize",
            "denied",
            e.description(),
            remote_for_error.clone(),
        );
        maybe_redirect(
            &e,
            Some(&redirect_for_error),
            state_for_error.as_deref(),
            &cfg.issuer,
            trusted,
        )
    };
    if !trusted {
        return fail(OAuthFail::InvalidRequest(
            "redirect_uri is not registered for this client".into(),
        ));
    }
    if q.response_type.as_deref() != Some("code") {
        return fail(OAuthFail::UnsupportedResponseType(
            "response_type must be code".into(),
        ));
    }
    let resource = match q.resource.as_deref() {
        Some(r) => r,
        None => {
            return fail(OAuthFail::InvalidRequest(
                "resource is required and must be this server's MCP URL".into(),
            ));
        }
    };
    match canonical_resource(resource) {
        Ok(got) if got == cfg.resource => {}
        Ok(_) => {
            return fail(OAuthFail::InvalidTarget(format!(
                "resource must be {}",
                cfg.resource
            )));
        }
        Err(m) => return fail(OAuthFail::InvalidTarget(m)),
    }
    let input = AuthorizeInput {
        client,
        redirect_uri,
        state: q.state,
        scope: q.scope,
        resource: cfg.resource.clone(),
        code_challenge: q.code_challenge.unwrap_or_default(),
        code_challenge_method: q.code_challenge_method.unwrap_or_default(),
        remote,
    };
    let store2 = store.clone();
    let begun = tokio::task::spawn_blocking(move || store2.begin_authorization(input)).await;
    match begun {
        Ok(Ok(pending)) => {
            eprintln!(
                "another: {} wants to connect (redirect host {}).\n  \
                 Approve it on this machine (this page cannot approve it):\n  \
                 another oauth approve {} --agent NAME --project '/path/*'",
                pending.client_name, pending.redirect_host, pending.user_code
            );
            let loc = format!(
                "/oauth/wait/{}?rt={}",
                pending.request_id, pending.redirect_token
            );
            no_store(Redirect::to(&loc).into_response())
        }
        Ok(Err(e)) => fail(e),
        Err(e) => fail(OAuthFail::Server(e.to_string())),
    }
}

async fn resolve_client(
    store: &Store,
    cfg: &OAuthConfig,
    client_id: &str,
) -> Result<ClientProfile, OAuthFail> {
    if client_id.is_empty() {
        return Err(OAuthFail::InvalidClient("client_id is required".into()));
    }
    if client_id.contains("://") {
        return fetch_cimd(cfg, client_id)
            .await
            .map_err(OAuthFail::InvalidClient);
    }
    let store = store.clone();
    let id = client_id.to_string();
    match tokio::task::spawn_blocking(move || store.oauth_client(&id)).await {
        Ok(Ok(Some(c))) => Ok(c),
        Ok(Ok(None)) => Err(OAuthFail::InvalidClient("unknown client_id".into())),
        Ok(Err(e)) => Err(OAuthFail::Server(e.to_string())),
        Err(e) => Err(OAuthFail::Server(e.to_string())),
    }
}

async fn fetch_cimd(cfg: &OAuthConfig, url: &str) -> Result<ClientProfile, String> {
    if let Ok(guard) = cfg.cimd.lock() {
        if let Some(hit) = guard.get(url) {
            if hit.expires > Instant::now() {
                return Ok(hit.profile.clone());
            }
        }
    }
    let allow_loopback = cfg.loopback;
    let url = url.to_string();
    let fetched = tokio::task::spawn_blocking(move || load_cimd_document(&url, allow_loopback))
        .await
        .map_err(|e| e.to_string())?;
    let (profile, ttl) = fetched?;
    if ttl > Duration::ZERO {
        remember_cimd(
            &cfg.cimd,
            profile.client_id.clone(),
            profile.clone(),
            Instant::now() + ttl,
        );
    }
    Ok(profile)
}

const CIMD_CACHE_MAX: usize = 32;
const CIMD_BODY_MAX: usize = 64 * 1024;

fn remember_cimd(
    cache: &Mutex<HashMap<String, CachedCimd>>,
    key: String,
    profile: ClientProfile,
    expires: Instant,
) {
    let Ok(mut guard) = cache.lock() else {
        return;
    };
    let now = Instant::now();
    guard.retain(|_, entry| entry.expires > now);
    if !guard.contains_key(&key) && guard.len() >= CIMD_CACHE_MAX {
        if let Some(oldest) = guard
            .iter()
            .min_by_key(|(_, entry)| entry.expires)
            .map(|(k, _)| k.clone())
        {
            guard.remove(&oldest);
        }
    }
    guard.insert(key, CachedCimd { profile, expires });
}

/// Connects only to an address resolved before the request, so a DNS change between
/// the check and the connect cannot retarget the fetch. TLS SNI and the Host header
/// stay the URL's hostname because the request URI is unchanged.
#[derive(Debug)]
struct PinnedResolver {
    addr: SocketAddr,
}

impl Resolver for PinnedResolver {
    fn resolve(
        &self,
        _uri: &ureq::http::Uri,
        _config: &ureq::config::Config,
        _timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let mut addrs = self.empty();
        addrs.push(self.addr);
        Ok(addrs)
    }
}

fn load_cimd_document(
    url: &str,
    allow_loopback: bool,
) -> Result<(ClientProfile, Duration), String> {
    let (scheme, host, port) = cimd_target(url, allow_loopback)?;
    let pinned = pin_public_address(&host, port, allow_loopback)?;
    let mut config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(5)))
        .max_redirects(0);
    if scheme == "https" {
        config = config.https_only(true);
    }
    let agent = ureq::Agent::with_parts(
        config.build(),
        DefaultConnector::default(),
        PinnedResolver { addr: pinned },
    );
    let mut resp = agent
        .get(url)
        .header("user-agent", "another-oauth")
        .header("accept", "application/json")
        .call()
        .map_err(|_| "client metadata document could not be fetched".to_string())?;
    if resp.status().as_u16() != 200 {
        return Err("client metadata document could not be fetched".into());
    }
    let ttl = cache_ttl(
        resp.headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
    );
    let bytes = read_capped(&mut resp.body_mut().as_reader(), CIMD_BODY_MAX)?;
    let text = String::from_utf8(bytes)
        .map_err(|_| "client metadata document is not UTF-8".to_string())?;
    let doc: Value = serde_json::from_str(&text)
        .map_err(|_| "client metadata document is not JSON".to_string())?;
    let profile = validate_cimd(url, &doc)?;
    Ok((profile, ttl))
}

fn read_capped(mut reader: impl Read, max: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut buf = [0u8; 2048];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|_| "client metadata document could not be read".to_string())?;
        if n == 0 {
            break;
        }
        if out.len().saturating_add(n) > max {
            return Err("client metadata document is too large".into());
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

fn cimd_target(url: &str, allow_loopback: bool) -> Result<(String, String, u16), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or("client_id must be an https URL with a path")?;
    let scheme = scheme.to_ascii_lowercase();
    if rest.contains('#') || rest.contains('?') || rest.contains('@') {
        return Err("client metadata URL must not include a query, fragment or userinfo".into());
    }
    let (authority, path) = rest
        .split_once('/')
        .ok_or("client_id URL must contain a path")?;
    if path.is_empty() {
        return Err("client_id URL must contain a path".into());
    }
    let (host, explicit_port) = split_authority(authority).map_err(|e| e.to_string())?;
    let https = scheme == "https";
    let loopback_http = scheme == "http" && allow_loopback && is_loopback_name(&host);
    if !https && !loopback_http {
        return Err("client_id must be an https URL".into());
    }
    let port = explicit_port.unwrap_or(if https { 443 } else { 80 });
    Ok((scheme, host, port))
}

fn pin_public_address(host: &str, port: u16, allow_loopback: bool) -> Result<SocketAddr, String> {
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|_| "client metadata host could not be resolved".to_string())?
        .collect();
    if addrs.is_empty() {
        return Err("client metadata host could not be resolved".into());
    }
    if addrs.iter().any(|a| !ip_allowed(a.ip(), allow_loopback)) {
        return Err("client metadata host is not a public address".into());
    }
    Ok(addrs[0])
}

/// Carrier-grade NAT, 100.64.0.0/10. Not covered by `Ipv4Addr::is_private` on Rust 1.88.
fn cgnat(v: std::net::Ipv4Addr) -> bool {
    let [a, b, _, _] = v.octets();
    a == 100 && (64..128).contains(&b)
}

fn ip_allowed(ip: IpAddr, allow_loopback: bool) -> bool {
    match ip {
        IpAddr::V4(v) => {
            if allow_loopback && v.is_loopback() {
                return true;
            }
            !(v.is_private()
                || v.is_loopback()
                || v.is_link_local()
                || v.is_broadcast()
                || v.is_documentation()
                || v.is_unspecified()
                || v.is_multicast()
                || cgnat(v)
                || v.octets()[0] == 0)
        }
        IpAddr::V6(v) => {
            if let Some(v4) = v.to_ipv4_mapped() {
                return ip_allowed(IpAddr::V4(v4), allow_loopback);
            }
            if allow_loopback && v.is_loopback() {
                return true;
            }
            !(v.is_loopback()
                || v.is_unspecified()
                || v.is_multicast()
                || v.is_unicast_link_local()
                || v.is_unique_local())
        }
    }
}

fn cache_ttl(header: Option<&str>) -> Duration {
    let Some(header) = header else {
        return Duration::from_secs(300);
    };
    let lower = header.to_ascii_lowercase();
    if lower.contains("no-store") {
        return Duration::ZERO;
    }
    let max_age = lower.split(',').find_map(|part| {
        let part = part.trim();
        let rest = part.strip_prefix("max-age=")?;
        rest.trim().parse::<u64>().ok()
    });
    match max_age {
        Some(0) => Duration::ZERO,
        Some(s) => Duration::from_secs(s.min(3600)),
        None => Duration::from_secs(300),
    }
}

#[derive(Debug, Deserialize)]
struct RedirectTokenQuery {
    rt: Option<String>,
}

async fn wait_page(
    State(store): State<Store>,
    Extension(ctx): Extension<AuthContext>,
    Path(request_id): Path<String>,
    Query(q): Query<RedirectTokenQuery>,
) -> Response {
    let Some(cfg) = cfg(&ctx) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let store2 = store.clone();
    let id = request_id.clone();
    let issuer = cfg.issuer.clone();
    let rt = q.rt.unwrap_or_default();
    let status =
        tokio::task::spawn_blocking(move || store2.authorization_status(&id, &rt, &issuer, false))
            .await;
    let Ok(Ok(status)) = status else {
        return fail_page(&OAuthFail::InvalidRequest("unknown authorization".into()));
    };
    let html = wait_html(&request_id, &status.status);
    let mut res = no_store(Html(html).into_response());
    res.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        header::HeaderValue::from_static(
            "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'",
        ),
    );
    res.headers_mut().insert(
        header::X_FRAME_OPTIONS,
        header::HeaderValue::from_static("DENY"),
    );
    res.headers_mut().insert(
        "referrer-policy",
        header::HeaderValue::from_static("no-referrer"),
    );
    res
}

fn wait_html(request_id: &str, status: &str) -> String {
    format!(
        r#"<!doctype html>
<meta charset="utf-8">
<title>Approve this client - Another</title>
<style>
  body {{ font: 16px/1.45 system-ui, sans-serif; max-width: 38rem; margin: 3rem auto; padding: 0 1rem; color: #1c1917; }}
  code, pre {{ font-family: ui-monospace, monospace; }}
  pre {{ background: #f5f5f4; padding: 0.8rem 1rem; overflow: auto; }}
  .warn {{ background: #fef3c7; padding: 0.6rem 0.8rem; }}
</style>
<h1>Approve this client on the Another machine</h1>
<p id="intro">This page cannot grant access. A person at the computer running Another approves the client, which then acts as exactly one agent.</p>
<p>Status: <strong id="state">{status}</strong></p>
<pre id="cmd">another oauth approve … --agent NAME --project '/path/*'</pre>
<p class="warn" id="warn" hidden>This client receives the code on a loopback address. Approve it only if you started the connection on this computer.</p>
<noscript><p>After you approve, reload this page. The app continues when the browser is sent back.</p></noscript>
<script>
const id = {id_json};
const rt = new URLSearchParams(location.search).get('rt') || '';
const state = document.getElementById('state');
async function poll() {{
  const res = await fetch('/oauth/pending/' + id + '?rt=' + encodeURIComponent(rt), {{ headers: {{ accept: 'application/json' }} }});
  if (!res.ok) {{ state.textContent = 'this request is not valid'; return; }}
  const body = await res.json();
  state.textContent = body.status;
  if (body.client_name) {{
    document.getElementById('intro').textContent =
      body.client_name + ' (redirect host ' + body.redirect_host + ') is waiting. This page cannot grant access.';
    document.getElementById('cmd').textContent =
      'another oauth approve ' + body.user_code + ' --agent NAME --project \'/path/*\'';
    document.getElementById('warn').hidden = !body.loopback_redirect;
  }}
  if (body.redirect) {{ location.replace(body.redirect); return; }}
  if (body.status === 'pending') setTimeout(poll, 2000);
}}
poll();
</script>
"#,
        status = esc(status),
        id_json = serde_json::to_string(request_id).unwrap_or_else(|_| "\"\"".into()),
    )
}

async fn pending(
    State(store): State<Store>,
    Extension(ctx): Extension<AuthContext>,
    Path(request_id): Path<String>,
    Query(q): Query<RedirectTokenQuery>,
) -> Response {
    let Some(cfg) = cfg(&ctx) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let store2 = store.clone();
    let issuer = cfg.issuer.clone();
    let id = request_id.clone();
    let rt = q.rt.unwrap_or_default();
    let status =
        tokio::task::spawn_blocking(move || store2.authorization_status(&id, &rt, &issuer, true))
            .await;
    let Ok(Ok(status)) = status else {
        return no_store(
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "unknown authorization"})),
            )
                .into_response(),
        );
    };
    // The poll response includes the user code so the page can show the exact command.
    // The request id in the URL is unguessable; the code still needs a local approval.
    let detail = {
        let store = store.clone();
        let id = request_id.clone();
        tokio::task::spawn_blocking(move || store.oauth_request_public(&id))
            .await
            .ok()
            .and_then(|r| r.ok())
            .flatten()
    };
    let mut body = json!({
        "status": status.status,
        "redirect": status.redirect,
    });
    if let Some(d) = detail {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("client_name".into(), json!(d.0));
            obj.insert("redirect_host".into(), json!(d.1));
            obj.insert("user_code".into(), json!(d.2));
            obj.insert("loopback_redirect".into(), json!(d.3));
        }
    }
    no_store(Json(body).into_response())
}

#[derive(Debug, Deserialize)]
struct RegisterBody {
    client_name: Option<String>,
    redirect_uris: Option<Vec<String>>,
    grant_types: Option<Vec<String>>,
    response_types: Option<Vec<String>>,
    token_endpoint_auth_method: Option<String>,
    application_type: Option<String>,
}

async fn register_client(
    State(store): State<Store>,
    Extension(ctx): Extension<AuthContext>,
    headers: HeaderMap,
    Json(body): Json<RegisterBody>,
) -> Response {
    if cfg(&ctx).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let remote = client_ip(&headers);
    if let Some(method) = body.token_endpoint_auth_method.as_deref() {
        if method != "none" {
            return oauth_json(&OAuthFail::InvalidRequest(
                "only public clients are accepted (token_endpoint_auth_method \"none\")".into(),
            ));
        }
    }
    if let Some(grants) = &body.grant_types {
        if grants
            .iter()
            .any(|g| g != "authorization_code" && g != "refresh_token")
        {
            return oauth_json(&OAuthFail::InvalidRequest(
                "grant_types must be authorization_code and refresh_token".into(),
            ));
        }
    }
    if let Some(types) = &body.response_types {
        if types.iter().any(|t| t != "code") {
            return oauth_json(&OAuthFail::InvalidRequest(
                "response_types must be code".into(),
            ));
        }
    }
    let reg = ClientRegistration {
        client_name: body.client_name.unwrap_or_default(),
        redirect_uris: body.redirect_uris.unwrap_or_default(),
        application_type: body.application_type,
        remote,
    };
    let store2 = store.clone();
    match tokio::task::spawn_blocking(move || store2.register_oauth_client(reg)).await {
        Ok(Ok(c)) => no_store(
            (
                StatusCode::CREATED,
                Json(json!({
                    "client_id": c.client_id,
                    "client_name": c.client_name,
                    "redirect_uris": c.redirect_uris,
                    "grant_types": ["authorization_code", "refresh_token"],
                    "response_types": ["code"],
                    "token_endpoint_auth_method": "none",
                    "application_type": c.application_type,
                })),
            )
                .into_response(),
        ),
        Ok(Err(e)) => oauth_json(&e),
        Err(e) => oauth_json(&OAuthFail::Server(e.to_string())),
    }
}

#[derive(Debug, Default)]
struct TokenForm {
    grant_type: Option<String>,
    code: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
    resource: Option<String>,
    client_secret: Option<String>,
    client_assertion: Option<String>,
}

async fn token(
    State(store): State<Store>,
    Extension(ctx): Extension<AuthContext>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let Some(cfg) = cfg(&ctx).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let ctype = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !ctype
        .to_ascii_lowercase()
        .starts_with("application/x-www-form-urlencoded")
    {
        return unsupported_media();
    }
    let form = token_form(&body);
    if form.client_secret.as_deref().is_some_and(|s| !s.is_empty())
        || form.client_assertion.is_some()
    {
        return oauth_json(&OAuthFail::InvalidClient(
            "this server only accepts public clients; do not send a client secret".into(),
        ));
    }
    let remote = client_ip(&headers);
    let client_id = form.client_id.clone().unwrap_or_default();
    let resource = match form.resource.clone() {
        Some(r) => r,
        None => {
            return oauth_json(&OAuthFail::InvalidRequest("resource is required".into()));
        }
    };
    if canonical_resource(&resource).ok().as_deref() != Some(cfg.resource.as_str()) {
        return oauth_json(&OAuthFail::InvalidTarget(format!(
            "resource must be {}",
            cfg.resource
        )));
    }
    let store2 = store.clone();
    let result = match form.grant_type.as_deref() {
        Some("authorization_code") => {
            let exchange = CodeExchange {
                code: form.code.unwrap_or_default(),
                client_id,
                redirect_uri: form.redirect_uri.unwrap_or_default(),
                code_verifier: form.code_verifier.unwrap_or_default(),
                resource,
                remote,
            };
            tokio::task::spawn_blocking(move || store2.exchange_code(exchange)).await
        }
        Some("refresh_token") => {
            let exchange = RefreshExchange {
                refresh_token: form.refresh_token.unwrap_or_default(),
                client_id,
                resource,
                remote,
            };
            tokio::task::spawn_blocking(move || store2.refresh_oauth(exchange)).await
        }
        _ => {
            return oauth_json(&OAuthFail::InvalidRequest(
                "grant_type must be authorization_code or refresh_token".into(),
            ));
        }
    };
    match result {
        Ok(Ok(tok)) => {
            let mut body = json!({
                "access_token": tok.access_token,
                "token_type": "Bearer",
                "expires_in": tok.expires_in,
                "scope": tok.scope,
            });
            if let Some(refresh) = tok.refresh_token {
                body["refresh_token"] = json!(refresh);
            }
            no_store(Json(body).into_response())
        }
        Ok(Err(e)) => oauth_json(&e),
        Err(e) => oauth_json(&OAuthFail::Server(e.to_string())),
    }
}

#[derive(Debug)]
struct RevokeForm {
    token: Option<String>,
}

async fn revoke(
    State(store): State<Store>,
    Extension(ctx): Extension<AuthContext>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if cfg(&ctx).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let ctype = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !ctype
        .to_ascii_lowercase()
        .starts_with("application/x-www-form-urlencoded")
    {
        return unsupported_media();
    }
    let values = form_pairs(&body);
    let form = RevokeForm {
        token: values.get("token").cloned(),
    };
    // RFC 7009: an invalid token still returns 200.
    if let Some(token) = form.token.filter(|t| !t.is_empty()) {
        let store2 = store.clone();
        let remote = client_ip(&headers);
        let _ =
            tokio::task::spawn_blocking(move || store2.revoke_oauth_token(&token, remote)).await;
    }
    no_store(StatusCode::OK.into_response())
}

fn oauth_json(err: &OAuthFail) -> Response {
    let status = StatusCode::from_u16(err.status()).unwrap_or(StatusCode::BAD_REQUEST);
    no_store(
        (
            status,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            Json(json!({
                "error": err.code(),
                "error_description": err.description(),
            })),
        )
            .into_response(),
    )
}

fn unsupported_media() -> Response {
    no_store(
        (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Json(json!({
                "error": "invalid_request",
                "error_description": "use application/x-www-form-urlencoded",
            })),
        )
            .into_response(),
    )
}

fn form_pairs(body: &[u8]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let text = String::from_utf8_lossy(body);
    for pair in text.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(percent_decode(k), percent_decode(v));
    }
    map
}

fn token_form(body: &[u8]) -> TokenForm {
    let v = form_pairs(body);
    let take = |k: &str| v.get(k).cloned();
    TokenForm {
        grant_type: take("grant_type"),
        code: take("code"),
        redirect_uri: take("redirect_uri"),
        client_id: take("client_id"),
        code_verifier: take("code_verifier"),
        refresh_token: take("refresh_token"),
        resource: take("resource"),
        client_secret: take("client_secret"),
        client_assertion: take("client_assertion"),
    }
}

fn percent_decode(raw: &str) -> String {
    let raw = raw.replace('+', " ");
    let bytes = raw.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(n) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(n);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_url_rules() {
        let cfg = parse_public_url("https://Another.Example.com/").unwrap();
        assert_eq!(cfg.issuer, "https://another.example.com");
        assert_eq!(cfg.resource, "https://another.example.com/mcp");
        assert!(!cfg.loopback);
        let local = parse_public_url("http://127.0.0.1:7467").unwrap();
        assert_eq!(local.resource, "http://127.0.0.1:7467/mcp");
        assert!(local.loopback);
        assert!(parse_public_url("http://another.example.com").is_err());
        assert!(parse_public_url("https://another.example.com/mcp").is_err());
    }

    #[test]
    fn host_allow_list_is_exact_about_ports() {
        let policy = HostPolicy::for_bind(7466, ["another.example.com", "edge.example:8443"]);
        assert!(policy.allows("127.0.0.1:7466"));
        assert!(policy.allows("localhost:7466"));
        assert!(policy.allows("[::1]:7466"));
        assert!(policy.allows("LOCALHOST:7466"));
        assert!(!policy.allows("127.0.0.1:9"));
        assert!(!policy.allows("localhost"));
        assert!(!policy.allows("localhost:7467"));
        assert!(!policy.allows("::1:7466"));
        // A configured host with no port matches only a Host header with no port.
        assert!(policy.allows("another.example.com"));
        assert!(policy.allows("Another.Example.com"));
        assert!(!policy.allows("another.example.com:443"));
        assert!(!policy.allows("another.example.com:8443"));
        assert!(policy.allows("edge.example:8443"));
        assert!(!policy.allows("edge.example"));
        assert!(!policy.allows("edge.example:443"));
        assert!(!policy.allows("notedge.example:8443"));
        assert!(!policy.allows("evil.example"));
        assert!(!policy.allows("evil.example:7466"));
        assert!(!policy.allows(""));
    }

    #[test]
    fn origin_match_is_exact_and_not_a_suffix() {
        let policy = OriginPolicy::for_bind(7466, ["https://another.example.com"]);
        assert!(policy.allows("http://127.0.0.1:7466"));
        assert!(policy.allows("HTTP://Localhost:7466"));
        assert!(policy.allows("http://[::1]:7466"));
        assert!(policy.allows("https://another.example.com"));
        assert!(policy.allows("https://Another.Example.com:443"));
        assert!(!policy.allows("https://another.example.com.evil.com"));
        assert!(!policy.allows("https://notanother.example.com"));
        assert!(!policy.allows("http://127.0.0.1:7466.evil.example"));
        assert!(!policy.allows("http://127.0.0.1:74660"));
        assert!(!policy.allows("http://127.0.0.1:9"));
        assert!(!policy.allows("https://127.0.0.1:7466"));
        assert!(!policy.allows("http://127.0.0.1:7466/mcp"));
        assert!(!policy.allows("http://evil.example"));
        assert!(!policy.allows("null"));
        assert!(!policy.allows(""));
    }

    #[test]
    fn rate_limiter_is_per_address_and_per_endpoint() {
        let limits = RateLimiter::new();
        let ip: IpAddr = "203.0.113.5".parse().unwrap();
        for _ in 0..TOKEN_BURST {
            assert!(limits.allow("token", ip).is_ok());
        }
        let retry = limits.allow("token", ip).unwrap_err();
        assert!(retry >= 1);
        assert!(limits.allow("register", ip).is_ok());
        let other: IpAddr = "203.0.113.6".parse().unwrap();
        assert!(limits.allow("token", other).is_ok());
    }

    #[test]
    fn metadata_host_must_be_public() {
        assert!(ip_allowed("8.8.8.8".parse().unwrap(), false));
        assert!(!ip_allowed("127.0.0.1".parse().unwrap(), false));
        assert!(ip_allowed("127.0.0.1".parse().unwrap(), true));
        assert!(!ip_allowed("10.1.2.3".parse().unwrap(), false));
        assert!(!ip_allowed("169.254.169.254".parse().unwrap(), false));
        assert!(!ip_allowed("192.168.1.5".parse().unwrap(), false));
        assert!(cimd_target("https://chatgpt.com/oauth/client.json", false).is_ok());
        assert!(cimd_target("http://127.0.0.1:9/client.json", false).is_err());
        assert!(cimd_target("http://127.0.0.1:9/client.json", true).is_ok());
        assert!(cimd_target("https://chatgpt.com", false).is_err());
        assert_eq!(
            cimd_target("https://example.com:8443/client.json", false)
                .unwrap()
                .2,
            8443
        );
        assert_eq!(
            cimd_target("https://example.com/client.json", false)
                .unwrap()
                .2,
            443
        );
        assert_eq!(
            cimd_target("http://127.0.0.1:9/client.json", true)
                .unwrap()
                .2,
            9
        );
    }

    #[test]
    fn pinned_resolver_does_not_look_up_the_url_again() {
        let addr: SocketAddr = "203.0.113.10:8443".parse().unwrap();
        let resolver = PinnedResolver { addr };
        let uri: ureq::http::Uri = "https://moved.example/client.json".parse().unwrap();
        let got = Resolver::resolve(
            &resolver,
            &uri,
            &ureq::Agent::config_builder().build(),
            NextTimeout::default(),
        )
        .unwrap();
        assert_eq!(got[0], addr);
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn metadata_body_stops_at_64_kib_and_cache_is_capped() {
        let mut big = vec![b'x'; CIMD_BODY_MAX + 1];
        assert!(read_capped(big.as_slice(), CIMD_BODY_MAX).is_err());
        big.pop();
        assert_eq!(
            read_capped(big.as_slice(), CIMD_BODY_MAX).unwrap().len(),
            CIMD_BODY_MAX
        );

        let cache = Mutex::new(HashMap::new());
        let profile = ClientProfile {
            client_id: "https://example.com/c.json".into(),
            client_name: "C".into(),
            redirect_uris: vec!["https://example.com/cb".into()],
        };
        for i in 0..CIMD_CACHE_MAX + 8 {
            remember_cimd(
                &cache,
                format!("https://example.com/{i}.json"),
                profile.clone(),
                Instant::now() + Duration::from_secs(60 + i as u64),
            );
        }
        assert_eq!(cache.lock().unwrap().len(), CIMD_CACHE_MAX);
    }

    #[test]
    fn metadata_fetch_connects_to_the_resolved_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/client.json");
        let body = format!(
            r#"{{"client_id":"{url}","client_name":"Loop","redirect_uris":["http://127.0.0.1/callback"],"token_endpoint_auth_method":"none"}}"#
        );
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 2048];
            let _ = std::io::Read::read(&mut sock, &mut buf);
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = std::io::Write::write_all(&mut sock, resp.as_bytes());
        });
        let (profile, _) = load_cimd_document(&url, true).unwrap();
        assert_eq!(profile.client_name, "Loop");
        // Port 443 on this host is not our server. A fetch that ignored the URL port
        // would not get this document.
        assert_ne!(addr.port(), 443);
    }
}
