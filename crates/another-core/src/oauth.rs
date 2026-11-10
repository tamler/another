//! OAuth 2.1 authorization for hosted MCP clients (ChatGPT, Claude, and others).
//!
//! The authorization server lives in the same process as the resource server. A grant is
//! created only by a local approval (`another oauth approve`); nothing in this module
//! approves a request on its own. Each grant acts as exactly one agent within project
//! globs, the same rule as a bearer token. Access tokens are short-lived. Refresh tokens
//! rotate, and presenting a replaced refresh token revokes the whole grant.
//!
//! Secrets (authorization codes, access tokens, refresh tokens, PKCE verifiers) are stored
//! only as SHA-256 hashes, except the authorization code itself, which is kept until the
//! browser picks it up and then cleared. Nothing here is written to the event log.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use globset::GlobBuilder;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::identity::resolve_agent;
use crate::store::{new_id, Store, Tx};
use crate::time::{fmt_opt, fmt_ts};
use crate::tokens::{AuditRecord, TokenScope};

/// Access tokens last one hour.
pub const ACCESS_TTL_SECS: u64 = 60 * 60;
/// Refresh tokens last 30 days and rotate on every use.
pub const REFRESH_TTL_SECS: u64 = 30 * 24 * 60 * 60;
/// An approved authorization code must be exchanged within this many seconds.
pub const CODE_TTL_SECS: u64 = 60;
/// A human has this long to approve a waiting client.
pub const PENDING_TTL_SECS: u64 = 10 * 60;

const MAX_DCR_CLIENTS: i64 = 200;
const MAX_PENDING: i64 = 50;
const ACCESS_PREFIX: &str = "anoat_";
const REFRESH_PREFIX: &str = "anort_";
const CODE_PREFIX: &str = "anoac_";

/// A client that may start an authorization request.
#[derive(Debug, Clone)]
pub struct ClientProfile {
    pub client_id: String,
    pub client_name: String,
    pub redirect_uris: Vec<String>,
}

/// Dynamic client registration input. Public clients only.
#[derive(Debug, Clone)]
pub struct ClientRegistration {
    pub client_name: String,
    pub redirect_uris: Vec<String>,
    /// `native` or `web`. Omitted means `web`, which rejects loopback redirects.
    pub application_type: Option<String>,
    pub remote: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegisteredClient {
    pub client_id: String,
    pub client_name: String,
    pub redirect_uris: Vec<String>,
    pub application_type: String,
}

/// What the authorization endpoint has already checked.
#[derive(Debug, Clone)]
pub struct AuthorizeInput {
    pub client: ClientProfile,
    pub redirect_uri: String,
    pub state: Option<String>,
    pub scope: Option<String>,
    pub resource: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub remote: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingAuthorization {
    pub request_id: String,
    pub user_code: String,
    pub client_name: String,
    pub redirect_host: String,
    pub loopback_redirect: bool,
    pub expires_at: String,
}

/// A waiting request, without the PKCE challenge or authorization code.
#[derive(Debug, Clone, Serialize)]
pub struct PendingView {
    pub user_code: String,
    pub client_name: String,
    pub redirect_host: String,
    pub created_at: String,
    pub expires_at: String,
}

#[derive(Debug, Clone)]
pub struct AuthorizationStatus {
    pub status: String,
    /// Where to send the browser once the request is decided. Never logged.
    pub redirect: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Approval {
    pub user_code: String,
    pub agent: String,
    pub project_globs: Vec<String>,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct OauthGrant {
    pub grant_id: String,
    pub client_id: String,
    pub client_name: String,
    pub agent: String,
    pub project_globs: Vec<String>,
    pub label: Option<String>,
    pub resource: String,
    pub scope: String,
    pub created_at: String,
    pub revoked_at: Option<String>,
    pub last_used_at: Option<String>,
}

/// Tokens just issued. Debug output omits the secrets.
pub struct IssuedTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: u64,
    pub scope: String,
    pub grant_id: String,
    pub agent: String,
}

impl std::fmt::Debug for IssuedTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedTokens")
            .field("grant_id", &self.grant_id)
            .field("agent", &self.agent)
            .field("expires_in", &self.expires_in)
            .field("scope", &self.scope)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct CodeExchange {
    pub code: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub code_verifier: String,
    pub resource: String,
    pub remote: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RefreshExchange {
    pub refresh_token: String,
    pub client_id: String,
    pub resource: String,
    pub remote: Option<String>,
}

/// An OAuth protocol error. `code` is the RFC 6749 error name.
#[derive(Debug)]
pub enum OAuthFail {
    InvalidRequest(String),
    InvalidClient(String),
    InvalidGrant(String),
    InvalidScope(String),
    InvalidTarget(String),
    UnsupportedResponseType(String),
    AccessDenied(String),
    TemporarilyUnavailable(String),
    Server(String),
}

impl OAuthFail {
    pub fn code(&self) -> &'static str {
        match self {
            OAuthFail::InvalidRequest(_) => "invalid_request",
            OAuthFail::InvalidClient(_) => "invalid_client",
            OAuthFail::InvalidGrant(_) => "invalid_grant",
            OAuthFail::InvalidScope(_) => "invalid_scope",
            OAuthFail::InvalidTarget(_) => "invalid_target",
            OAuthFail::UnsupportedResponseType(_) => "unsupported_response_type",
            OAuthFail::AccessDenied(_) => "access_denied",
            OAuthFail::TemporarilyUnavailable(_) => "temporarily_unavailable",
            OAuthFail::Server(_) => "server_error",
        }
    }

    pub fn description(&self) -> &str {
        match self {
            OAuthFail::InvalidRequest(m)
            | OAuthFail::InvalidClient(m)
            | OAuthFail::InvalidGrant(m)
            | OAuthFail::InvalidScope(m)
            | OAuthFail::InvalidTarget(m)
            | OAuthFail::UnsupportedResponseType(m)
            | OAuthFail::AccessDenied(m)
            | OAuthFail::TemporarilyUnavailable(m)
            | OAuthFail::Server(m) => m,
        }
    }

    /// HTTP status for a token-endpoint response.
    pub fn status(&self) -> u16 {
        match self {
            OAuthFail::InvalidClient(_) => 401,
            OAuthFail::Server(_) => 500,
            _ => 400,
        }
    }
}

impl From<Error> for OAuthFail {
    fn from(e: Error) -> Self {
        OAuthFail::Server(e.to_string())
    }
}

fn hash_secret(secret: &str) -> String {
    Sha256::digest(secret.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn secrets_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

fn random_bytes(n: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf)
        .map_err(|e| Error::Invalid(format!("no system randomness available: {e}")))?;
    Ok(buf)
}

fn random_hex(n: usize) -> Result<String> {
    Ok(random_bytes(n)?
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn user_code() -> Result<String> {
    // 32 divides 256, so taking the low 5 bits is unbiased.
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let bytes = random_bytes(8)?;
    let chars: String = bytes
        .iter()
        .map(|b| ALPHABET[(b & 31) as usize] as char)
        .collect();
    Ok(format!("{}-{}", &chars[..4], &chars[4..]))
}

fn normalize_user_code(raw: &str) -> String {
    raw.trim().to_ascii_lowercase().replace([' ', '_'], "-")
}

fn insert_audit(tx: &Tx<'_>, entry: AuditRecord) -> Result<()> {
    tx.execute(
        "INSERT INTO audit_log (node_id, ts, token_id, agent, operation, outcome, detail, remote)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            tx.node_id,
            tx.now,
            entry.token_id,
            entry.agent,
            entry.operation,
            entry.outcome,
            entry.detail,
            entry.remote
        ],
    )?;
    Ok(())
}

/// Canonical resource identifier (RFC 8707): lowercase scheme and host, no fragment,
/// no query, no userinfo, default ports omitted, one trailing slash stripped.
pub fn canonical_resource(raw: &str) -> std::result::Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.chars().any(|c| c.is_whitespace()) {
        return Err("resource must be an absolute URI".into());
    }
    let Some((scheme, rest)) = raw.split_once("://") else {
        return Err("resource must be an absolute URI".into());
    };
    if rest.contains('#') || rest.contains('?') || rest.contains('@') {
        return Err("resource must not include a fragment, query or userinfo".into());
    }
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "https" && scheme != "http" {
        return Err("resource scheme must be https or http".into());
    }
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Err("resource is missing a host".into());
    }
    let (host, port) = split_host_port(authority)?;
    let port = match (scheme.as_str(), port) {
        ("https", Some(443)) | ("http", Some(80)) => None,
        (_, other) => other,
    };
    let path = if path.len() > 1 {
        path.trim_end_matches('/')
    } else {
        path
    };
    Ok(format!(
        "{scheme}://{}{path}",
        format_authority(&host, port)
    ))
}

fn format_authority(host: &str, port: Option<u16>) -> String {
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

fn split_host_port(authority: &str) -> std::result::Result<(String, Option<u16>), String> {
    if let Some(rest) = authority.strip_prefix('[') {
        let Some((host, after)) = rest.split_once(']') else {
            return Err("invalid host".into());
        };
        if host.is_empty() {
            return Err("resource is missing a host".into());
        }
        let port = if after.is_empty() {
            None
        } else if let Some(p) = after.strip_prefix(':') {
            Some(parse_port(p)?)
        } else {
            return Err("invalid host".into());
        };
        return Ok((host.to_ascii_lowercase(), port));
    }
    if authority.matches(':').count() > 1 {
        return Err("invalid host".into());
    }
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => (h, Some(parse_port(p)?)),
        None => (authority, None),
    };
    if host.is_empty() {
        return Err("resource is missing a host".into());
    }
    Ok((host.to_ascii_lowercase(), port))
}

fn parse_port(raw: &str) -> std::result::Result<u16, String> {
    raw.parse::<u16>()
        .map_err(|_| "invalid port".to_string())
        .and_then(|p| {
            if p == 0 {
                Err("invalid port".into())
            } else {
                Ok(p)
            }
        })
}

#[derive(Debug, Clone)]
struct ParsedRedirect {
    scheme: String,
    host: String,
    #[allow(dead_code)]
    port: Option<u16>,
    path: String,
    query: Option<String>,
}

fn parse_redirect(raw: &str) -> Option<ParsedRedirect> {
    if raw.contains('#') || raw.contains('@') || raw.chars().any(|c| c.is_whitespace()) {
        return None;
    }
    let (scheme, rest) = raw.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "https" && scheme != "http" {
        return None;
    }
    let (authority, rest) = match rest.find(['/', '?']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return None;
    }
    let (host, port) = split_host_port(authority).ok()?;
    let (path, query) = match rest.split_once('?') {
        Some((p, q)) => (p.to_string(), Some(q.to_string())),
        None => (
            if rest.is_empty() {
                "/".into()
            } else {
                rest.into()
            },
            None,
        ),
    };
    Some(ParsedRedirect {
        scheme,
        host,
        port,
        path,
        query,
    })
}

fn is_loopback_host(host: &str) -> bool {
    host == "localhost" || host == "127.0.0.1" || host == "::1"
}

/// Whether a redirect URI is acceptable at registration time.
pub fn validate_redirect_uri(uri: &str, application_type: &str) -> std::result::Result<(), String> {
    let parsed =
        parse_redirect(uri).ok_or_else(|| format!("redirect URI {uri:?} is not allowed"))?;
    let loopback = parsed.scheme == "http" && is_loopback_host(&parsed.host);
    if parsed.scheme == "https" || (application_type == "native" && loopback) {
        return Ok(());
    }
    if loopback {
        return Err(format!(
            "redirect URI {uri:?} is loopback; register application_type \"native\" (web clients must use https)"
        ));
    }
    Err(format!(
        "redirect URI {uri:?} must be https, or http loopback for a native client"
    ))
}

/// Exact match, plus the RFC 8252 loopback rule: a registered loopback URI matches the
/// same scheme, host and path on any port. `localhost` and `127.0.0.1` are not aliases.
pub fn redirect_matches(registered: &[String], requested: &str) -> bool {
    if registered.iter().any(|r| r == requested) {
        return true;
    }
    let Some(req) = parse_redirect(requested) else {
        return false;
    };
    if req.scheme != "http" || !is_loopback_host(&req.host) {
        return false;
    }
    registered.iter().any(|raw| {
        let Some(reg) = parse_redirect(raw) else {
            return false;
        };
        reg.scheme == "http"
            && is_loopback_host(&reg.host)
            && reg.host == req.host
            && reg.path == req.path
            && reg.query == req.query
    })
}

pub fn redirect_host(uri: &str) -> String {
    parse_redirect(uri)
        .map(|p| p.host)
        .unwrap_or_else(|| "unknown".into())
}

/// Authorization error redirect, including the issuer (`iss`) on every response.
pub fn error_redirect(
    uri: &str,
    error: &str,
    description: &str,
    state: Option<&str>,
    issuer: &str,
) -> String {
    let mut pairs = vec![("error", error), ("error_description", description)];
    if let Some(s) = state {
        pairs.push(("state", s));
    }
    pairs.push(("iss", issuer));
    redirect_back(uri, &pairs)
}

pub fn redirect_is_loopback(uri: &str) -> bool {
    parse_redirect(uri)
        .map(|p| p.scheme == "http" && is_loopback_host(&p.host))
        .unwrap_or(false)
}

/// Check a Client ID Metadata Document. `document_url` is the URL the client presented.
pub fn validate_cimd(
    document_url: &str,
    doc: &serde_json::Value,
) -> std::result::Result<ClientProfile, String> {
    let obj = doc
        .as_object()
        .ok_or("client metadata must be a JSON object")?;
    let client_id = obj
        .get("client_id")
        .and_then(|v| v.as_str())
        .ok_or("client metadata is missing client_id")?;
    if client_id != document_url {
        return Err("client_id does not match the metadata document URL".into());
    }
    let client_name = obj
        .get("client_name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("client metadata is missing client_name")?;
    if client_name.chars().count() > 120 {
        return Err("client_name is too long".into());
    }
    let uris = obj
        .get("redirect_uris")
        .and_then(|v| v.as_array())
        .ok_or("client metadata is missing redirect_uris")?;
    if uris.is_empty() || uris.len() > 10 {
        return Err("redirect_uris must contain 1 to 10 URIs".into());
    }
    let mut redirect_uris = Vec::new();
    for u in uris {
        let u = u
            .as_str()
            .ok_or("redirect_uris must be strings")?
            .to_string();
        // CIMD clients may be native (loopback) or web (https). Accept either shape here;
        // each URI is still checked on its own.
        let native_ok = validate_redirect_uri(&u, "native").is_ok();
        if !native_ok {
            return Err(format!("redirect URI {u:?} is not allowed"));
        }
        redirect_uris.push(u);
    }
    let method = obj
        .get("token_endpoint_auth_method")
        .and_then(|v| v.as_str());
    let methods = obj
        .get("token_endpoint_auth_methods_supported")
        .and_then(|v| v.as_array());
    let allows_none = method.is_none_or(|m| m == "none")
        || methods.is_some_and(|ms| ms.iter().any(|m| m.as_str() == Some("none")));
    if !allows_none {
        return Err(
            "this server only accepts public clients (token endpoint auth method \"none\")".into(),
        );
    }
    Ok(ClientProfile {
        client_id: client_id.to_string(),
        client_name: client_name.to_string(),
        redirect_uris,
    })
}

fn normalize_scope(raw: Option<&str>) -> std::result::Result<String, OAuthFail> {
    let raw = raw.unwrap_or("another").trim();
    if raw.is_empty() {
        return Ok("another".into());
    }
    let mut out = Vec::new();
    for scope in raw.split_whitespace() {
        match scope {
            "another" | "offline_access" => {
                if !out.contains(&scope) {
                    out.push(scope);
                }
            }
            other => {
                return Err(OAuthFail::InvalidScope(format!(
                    "unknown scope {other:?}; this server grants \"another\" and \"offline_access\""
                )));
            }
        }
    }
    if !out.contains(&"another") {
        return Err(OAuthFail::InvalidScope(
            "scope must include \"another\"".into(),
        ));
    }
    Ok(out.join(" "))
}

fn pkce_s256(verifier: &str) -> std::result::Result<String, OAuthFail> {
    let len = verifier.len();
    if !(43..=128).contains(&len)
        || !verifier.bytes().all(
            |b| matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~'),
        )
    {
        return Err(OAuthFail::InvalidGrant(
            "code_verifier must be 43 to 128 unreserved characters".into(),
        ));
    }
    Ok(URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())))
}

fn form_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn append_query(url: &str, pairs: &[(&str, &str)]) -> String {
    let mut out = url.to_string();
    let mut first = !url.contains('?');
    for (k, v) in pairs {
        out.push(if first { '?' } else { '&' });
        first = false;
        out.push_str(&form_encode(k));
        out.push('=');
        out.push_str(&form_encode(v));
    }
    out
}

fn redirect_back(uri: &str, pairs: &[(&str, &str)]) -> String {
    append_query(uri, pairs)
}

struct RefreshRow {
    grant_id: String,
    secret_hash: String,
    resource: String,
    scope: String,
    expires_at: i64,
    revoked_at: Option<i64>,
    replaced_by: Option<String>,
}

struct RequestRow {
    request_id: String,
    client_id: String,
    client_name: String,
    redirect_uri: String,
    state: Option<String>,
    scope: String,
    resource: String,
    code_challenge: String,
    code_challenge_method: String,
    status: String,
    grant_id: Option<String>,
    code: Option<String>,
    code_hash: Option<String>,
    expires_at: i64,
    code_expires_at: Option<i64>,
}

fn load_request(tx: &Tx<'_>, request_id: &str) -> Result<Option<RequestRow>> {
    tx.query_row(
        "SELECT request_id, client_id, client_name, redirect_uri, state, scope, resource,
                code_challenge, code_challenge_method, status, grant_id, code, code_hash,
                expires_at, code_expires_at
         FROM oauth_requests WHERE request_id = ?1 AND node_id = ?2",
        params![request_id, tx.node_id],
        |r| {
            Ok(RequestRow {
                request_id: r.get(0)?,
                client_id: r.get(1)?,
                client_name: r.get(2)?,
                redirect_uri: r.get(3)?,
                state: r.get(4)?,
                scope: r.get(5)?,
                resource: r.get(6)?,
                code_challenge: r.get(7)?,
                code_challenge_method: r.get(8)?,
                status: r.get(9)?,
                grant_id: r.get(10)?,
                code: r.get(11)?,
                code_hash: r.get(12)?,
                expires_at: r.get(13)?,
                code_expires_at: r.get(14)?,
            })
        },
    )
    .optional()
    .map_err(Error::from)
}

fn grant_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<OauthGrant> {
    let globs: String = r.get(5)?;
    Ok(OauthGrant {
        grant_id: r.get(0)?,
        client_id: r.get(1)?,
        client_name: r.get(2)?,
        agent: r.get(3)?,
        project_globs: serde_json::from_str(&globs).unwrap_or_default(),
        label: r.get(6)?,
        resource: r.get(7)?,
        scope: r.get(8)?,
        created_at: fmt_ts(r.get(9)?),
        revoked_at: fmt_opt(r.get(10)?),
        last_used_at: fmt_opt(r.get(11)?),
    })
}

const GRANT_SELECT: &str = "SELECT g.grant_id, g.client_id, g.client_name, a.address, NULL,
       g.project_globs, g.label, g.resource, g.scope, g.created_at, g.revoked_at, g.last_used_at
     FROM oauth_grants g JOIN agents a ON a.agent_id = g.agent_id
     LEFT JOIN projects p ON p.project_id = a.project_id";

fn load_grant(tx: &Tx<'_>, grant_id: &str) -> Result<OauthGrant> {
    tx.query_row(
        &format!("{GRANT_SELECT} WHERE g.grant_id = ?1 AND g.node_id = ?2"),
        params![grant_id, tx.node_id],
        grant_from_row,
    )
    .optional()?
    .ok_or_else(|| Error::NotFound(format!("unknown grant {grant_id:?}")))
}

fn check_globs(globs: &[String]) -> Result<()> {
    for g in globs {
        GlobBuilder::new(g)
            .literal_separator(true)
            .build()
            .map_err(|e| Error::Invalid(format!("invalid project glob {g:?}: {e}")))?;
    }
    Ok(())
}

fn insert_token(
    tx: &Tx<'_>,
    grant_id: &str,
    kind: &str,
    prefix: &str,
    resource: &str,
    scope: &str,
    ttl_secs: u64,
) -> Result<String> {
    let token_id = new_id();
    let hex = random_hex(32)?;
    let secret = format!("{prefix}{token_id}_{hex}");
    let expires_at = tx.now + (ttl_secs as i64) * 1000;
    tx.execute(
        "INSERT INTO oauth_tokens (token_id, node_id, grant_id, kind, secret_hash, resource, scope,
            expires_at, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            token_id,
            tx.node_id,
            grant_id,
            kind,
            hash_secret(&secret),
            resource,
            scope,
            expires_at,
            tx.now
        ],
    )?;
    Ok(secret)
}

fn issue_pair(tx: &Tx<'_>, grant_id: &str, resource: &str, scope: &str) -> Result<IssuedTokens> {
    let access = insert_token(
        tx,
        grant_id,
        "access",
        ACCESS_PREFIX,
        resource,
        scope,
        ACCESS_TTL_SECS,
    )?;
    let refresh = insert_token(
        tx,
        grant_id,
        "refresh",
        REFRESH_PREFIX,
        resource,
        scope,
        REFRESH_TTL_SECS,
    )?;
    let agent = load_grant(tx, grant_id)?.agent;
    tx.execute(
        "UPDATE oauth_grants SET last_used_at = ?2 WHERE grant_id = ?1",
        params![grant_id, tx.now],
    )?;
    Ok(IssuedTokens {
        access_token: access,
        refresh_token: Some(refresh),
        expires_in: ACCESS_TTL_SECS,
        scope: scope.to_string(),
        grant_id: grant_id.to_string(),
        agent,
    })
}

fn with_iss<'a>(
    pairs: Vec<(&'a str, &'a str)>,
    state: &'a Option<String>,
    issuer: &'a str,
) -> Vec<(&'a str, &'a str)> {
    let mut pairs = pairs;
    if let Some(s) = state {
        pairs.push(("state", s));
    }
    pairs.push(("iss", issuer));
    pairs
}

fn auth_redirect(req: &RequestRow, status: &str, now: i64, issuer: &str) -> Option<String> {
    match status {
        "approved" => {
            let fresh = req.code_expires_at.is_some_and(|t| t > now);
            match (fresh, req.code.as_deref()) {
                (true, Some(code)) => Some(redirect_back(
                    &req.redirect_uri,
                    &with_iss(vec![("code", code)], &req.state, issuer),
                )),
                _ => Some(redirect_back(
                    &req.redirect_uri,
                    &with_iss(
                        vec![
                            ("error", "invalid_request"),
                            (
                                "error_description",
                                "the approval expired before the app collected it; connect again",
                            ),
                        ],
                        &req.state,
                        issuer,
                    ),
                )),
            }
        }
        "denied" => Some(redirect_back(
            &req.redirect_uri,
            &with_iss(
                vec![
                    ("error", "access_denied"),
                    ("error_description", "the operator denied this client"),
                ],
                &req.state,
                issuer,
            ),
        )),
        _ => None,
    }
}

fn revoke_grant_tx(tx: &Tx<'_>, grant_id: &str) -> Result<()> {
    tx.execute(
        "UPDATE oauth_grants SET revoked_at = ?2 WHERE grant_id = ?1 AND revoked_at IS NULL",
        params![grant_id, tx.now],
    )?;
    tx.execute(
        "UPDATE oauth_tokens SET revoked_at = ?2
         WHERE grant_id = ?1 AND revoked_at IS NULL",
        params![grant_id, tx.now],
    )?;
    Ok(())
}

fn split_prefixed<'a>(secret: &'a str, prefix: &str) -> Option<(&'a str, &'a str)> {
    let rest = secret.strip_prefix(prefix)?;
    let (id, hex) = rest.split_once('_')?;
    if id.is_empty() || hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some((id, secret))
}

impl Store {
    pub fn register_oauth_client(
        &self,
        reg: ClientRegistration,
    ) -> std::result::Result<RegisteredClient, OAuthFail> {
        let name = reg.client_name.trim();
        if name.is_empty() || name.chars().count() > 120 {
            return Err(OAuthFail::InvalidRequest(
                "client_name is required and must be at most 120 characters".into(),
            ));
        }
        if reg.redirect_uris.is_empty() || reg.redirect_uris.len() > 10 {
            return Err(OAuthFail::InvalidRequest(
                "redirect_uris must contain 1 to 10 URIs".into(),
            ));
        }
        let application_type = match reg.application_type.as_deref().unwrap_or("web") {
            "web" => "web",
            "native" => "native",
            other => {
                return Err(OAuthFail::InvalidRequest(format!(
                    "application_type {other:?} is not supported; use \"web\" or \"native\""
                )));
            }
        };
        for uri in &reg.redirect_uris {
            validate_redirect_uri(uri, application_type).map_err(OAuthFail::InvalidRequest)?;
        }
        let client_id = format!("anoc_{}", new_id());
        let uris = serde_json::to_string(&reg.redirect_uris)
            .map_err(|e| OAuthFail::Server(e.to_string()))?;
        let remote = reg.remote.clone();
        self.write(|tx| {
            let n: i64 = tx.query_row(
                "SELECT count(*) FROM oauth_clients WHERE node_id = ?1",
                [tx.node_id],
                |r| r.get(0),
            )?;
            if n >= MAX_DCR_CLIENTS {
                return Err(Error::Invalid(
                    "too many registered clients on this node".into(),
                ));
            }
            tx.execute(
                "INSERT INTO oauth_clients (client_id, node_id, client_name, redirect_uris,
                    application_type, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![client_id, tx.node_id, name, uris, application_type, tx.now],
            )?;
            insert_audit(
                tx,
                AuditRecord {
                    operation: "oauth.register".into(),
                    outcome: "ok".into(),
                    detail: Some(format!(
                        "client {client_id} name {name:?} redirects {}",
                        reg.redirect_uris
                            .iter()
                            .map(|u| redirect_host(u))
                            .collect::<Vec<_>>()
                            .join(",")
                    )),
                    remote,
                    ..Default::default()
                },
            )?;
            Ok(())
        })
        .map_err(|e| match e {
            Error::Invalid(m) => OAuthFail::TemporarilyUnavailable(m),
            other => OAuthFail::from(other),
        })?;
        Ok(RegisteredClient {
            client_id,
            client_name: name.to_string(),
            redirect_uris: reg.redirect_uris,
            application_type: application_type.to_string(),
        })
    }

    pub fn oauth_client(&self, client_id: &str) -> Result<Option<ClientProfile>> {
        self.read(|conn| {
            let row: Option<(String, String)> = conn
                .query_row(
                    "SELECT client_name, redirect_uris FROM oauth_clients
                     WHERE client_id = ?1 AND node_id = ?2",
                    params![client_id, self.node_id()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            Ok(row.map(|(name, uris)| ClientProfile {
                client_id: client_id.to_string(),
                client_name: name,
                redirect_uris: serde_json::from_str(&uris).unwrap_or_default(),
            }))
        })
    }

    pub fn begin_authorization(
        &self,
        input: AuthorizeInput,
    ) -> std::result::Result<PendingAuthorization, OAuthFail> {
        if input.code_challenge_method != "S256" {
            return Err(OAuthFail::InvalidRequest(
                "code_challenge_method must be S256".into(),
            ));
        }
        if !(43..=128).contains(&input.code_challenge.len()) {
            return Err(OAuthFail::InvalidRequest(
                "code_challenge is missing or malformed".into(),
            ));
        }
        if !redirect_matches(&input.client.redirect_uris, &input.redirect_uri) {
            return Err(OAuthFail::InvalidRequest(
                "redirect_uri is not registered for this client".into(),
            ));
        }
        let scope = normalize_scope(input.scope.as_deref())?;
        let resource = canonical_resource(&input.resource).map_err(OAuthFail::InvalidTarget)?;
        let request_id = random_hex(32)?;
        let user_code = user_code()?;
        let remote = input.remote.clone();
        let client_name = input.client.client_name.clone();
        let redirect_host = redirect_host(&input.redirect_uri);
        let stored_id = request_id.clone();
        let stored_code = user_code.clone();
        self.write(|tx| {
            let waiting: i64 = tx.query_row(
                "SELECT count(*) FROM oauth_requests
                 WHERE node_id = ?1 AND status = 'pending' AND expires_at > ?2",
                params![tx.node_id, tx.now],
                |r| r.get(0),
            )?;
            if waiting >= MAX_PENDING {
                return Err(Error::Invalid(
                    "too many clients are waiting for approval".into(),
                ));
            }
            let expires_at = tx.now + (PENDING_TTL_SECS as i64) * 1000;
            tx.execute(
                "INSERT INTO oauth_requests (request_id, node_id, client_id, client_name,
                    redirect_uri, state, scope, resource, code_challenge, code_challenge_method,
                    user_code, status, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'pending', ?12, ?13)",
                params![
                    stored_id,
                    tx.node_id,
                    input.client.client_id,
                    client_name,
                    input.redirect_uri,
                    input.state,
                    scope,
                    resource,
                    input.code_challenge,
                    input.code_challenge_method,
                    stored_code,
                    tx.now,
                    expires_at
                ],
            )?;
            insert_audit(
                tx,
                AuditRecord {
                    operation: "oauth.authorize".into(),
                    outcome: "pending".into(),
                    detail: Some(format!(
                        "client {client_name:?} redirect host {redirect_host}"
                    )),
                    remote,
                    ..Default::default()
                },
            )?;
            Ok(expires_at)
        })
        .map_err(|e| match e {
            Error::Invalid(m) => OAuthFail::TemporarilyUnavailable(m),
            other => OAuthFail::from(other),
        })
        .map(|expires_at| PendingAuthorization {
            request_id,
            user_code,
            client_name,
            redirect_host,
            loopback_redirect: redirect_is_loopback(&input.redirect_uri),
            expires_at: fmt_ts(expires_at),
        })
    }

    /// Local approval. This is the only path that creates a grant.
    pub fn approve_oauth(&self, approval: Approval) -> Result<OauthGrant> {
        check_globs(&approval.project_globs)?;
        let code_key = normalize_user_code(&approval.user_code);
        let label = approval.label.clone();
        let globs = approval.project_globs.clone();
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, &approval.agent)?;
            let row: Option<(String, String, i64)> = tx
                .query_row(
                    "SELECT request_id, status, expires_at FROM oauth_requests
                     WHERE user_code = ?1 AND node_id = ?2",
                    params![code_key, tx.node_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let Some((request_id, status, expires_at)) = row else {
                return Err(Error::NotFound(format!(
                    "no client is waiting with code {code_key:?}. Run `another oauth pending`."
                )));
            };
            if status != "pending" {
                return Err(Error::Conflict(format!(
                    "code {code_key} is already {status}"
                )));
            }
            if expires_at <= tx.now {
                tx.execute(
                    "UPDATE oauth_requests SET status = 'expired' WHERE request_id = ?1",
                    [&request_id],
                )?;
                return Err(Error::Invalid(format!(
                    "code {code_key} expired; ask the app to connect again"
                )));
            }
            let req = load_request(tx, &request_id)?.expect("request exists");
            let grant_id = new_id();
            let secret = random_hex(32)?;
            let code = format!("{CODE_PREFIX}{request_id}_{secret}");
            let code_expires = tx.now + (CODE_TTL_SECS as i64) * 1000;
            tx.execute(
                "INSERT INTO oauth_grants (grant_id, node_id, client_id, client_name, agent_id,
                    project_globs, label, resource, scope, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    grant_id,
                    tx.node_id,
                    req.client_id,
                    req.client_name,
                    me.id,
                    serde_json::to_string(&globs)?,
                    label,
                    req.resource,
                    req.scope,
                    tx.now
                ],
            )?;
            tx.execute(
                "UPDATE oauth_requests
                 SET status = 'approved', grant_id = ?2, code = ?3, code_hash = ?4,
                     code_expires_at = ?5
                 WHERE request_id = ?1",
                params![request_id, grant_id, code, hash_secret(&code), code_expires],
            )?;
            insert_audit(
                tx,
                AuditRecord {
                    token_id: Some(grant_id.clone()),
                    agent: Some(me.address.clone()),
                    operation: "oauth.approve".into(),
                    outcome: "ok".into(),
                    detail: Some(format!(
                        "client {:?} redirect host {} globs {}",
                        req.client_name,
                        redirect_host(&req.redirect_uri),
                        if globs.is_empty() {
                            "none".to_string()
                        } else {
                            globs.join(",")
                        }
                    )),
                    ..Default::default()
                },
            )?;
            load_grant(tx, &grant_id)
        })
    }

    pub fn deny_oauth(&self, user_code: &str) -> Result<PendingView> {
        let code_key = normalize_user_code(user_code);
        self.write(|tx| {
            let row: Option<(String, String)> = tx
                .query_row(
                    "SELECT request_id, status FROM oauth_requests
                     WHERE user_code = ?1 AND node_id = ?2",
                    params![code_key, tx.node_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((request_id, status)) = row else {
                return Err(Error::NotFound(format!(
                    "no client is waiting with code {code_key:?}"
                )));
            };
            if status != "pending" {
                return Err(Error::Conflict(format!(
                    "code {code_key} is already {status}"
                )));
            }
            tx.execute(
                "UPDATE oauth_requests SET status = 'denied' WHERE request_id = ?1",
                [&request_id],
            )?;
            let req = load_request(tx, &request_id)?.expect("request exists");
            insert_audit(
                tx,
                AuditRecord {
                    operation: "oauth.deny".into(),
                    outcome: "denied".into(),
                    detail: Some(format!(
                        "client {:?} redirect host {}",
                        req.client_name,
                        redirect_host(&req.redirect_uri)
                    )),
                    ..Default::default()
                },
            )?;
            Ok(PendingView {
                user_code: code_key,
                client_name: req.client_name,
                redirect_host: redirect_host(&req.redirect_uri),
                created_at: String::new(),
                expires_at: String::new(),
            })
        })
    }

    pub fn oauth_pending(&self) -> Result<Vec<PendingView>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT user_code, client_name, redirect_uri, created_at, expires_at
                 FROM oauth_requests
                 WHERE node_id = ?1 AND status = 'pending' AND expires_at > ?2
                 ORDER BY created_at",
            )?;
            let now = self.now();
            let rows = stmt.query_map(params![self.node_id(), now], |r| {
                let uri: String = r.get(2)?;
                Ok(PendingView {
                    user_code: r.get(0)?,
                    client_name: r.get(1)?,
                    redirect_host: redirect_host(&uri),
                    created_at: fmt_ts(r.get(3)?),
                    expires_at: fmt_ts(r.get(4)?),
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn oauth_grants(&self) -> Result<Vec<OauthGrant>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(&format!(
                "{GRANT_SELECT} WHERE g.node_id = ?1 ORDER BY g.created_at DESC"
            ))?;
            let rows = stmt.query_map([self.node_id()], grant_from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn revoke_oauth_grant(&self, grant_id: &str) -> Result<OauthGrant> {
        self.write(|tx| {
            let grant = load_grant(tx, grant_id)?;
            if grant.revoked_at.is_none() {
                revoke_grant_tx(tx, grant_id)?;
                insert_audit(
                    tx,
                    AuditRecord {
                        token_id: Some(grant_id.to_string()),
                        agent: Some(grant.agent.clone()),
                        operation: "oauth.revoke".into(),
                        outcome: "ok".into(),
                        detail: Some(format!("client {}", grant.client_name)),
                        ..Default::default()
                    },
                )?;
            }
            load_grant(tx, grant_id)
        })
    }

    /// Names shown on the waiting page. Does not include the authorization code.
    pub fn oauth_request_public(
        &self,
        request_id: &str,
    ) -> Result<Option<(String, String, String, bool)>> {
        if request_id.len() != 64 || !request_id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(None);
        }
        self.read(|conn| {
            let row: Option<(String, String, String)> = conn
                .query_row(
                    "SELECT client_name, redirect_uri, user_code FROM oauth_requests
                     WHERE request_id = ?1 AND node_id = ?2",
                    params![request_id, self.node_id()],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            Ok(row.map(|(name, uri, code)| {
                (name, redirect_host(&uri), code, redirect_is_loopback(&uri))
            }))
        })
    }

    /// Browser poll. Returns a redirect only after a local decision, and only to the
    /// redirect URI stored with the request.
    pub fn authorization_status(
        &self,
        request_id: &str,
        issuer: &str,
    ) -> std::result::Result<AuthorizationStatus, OAuthFail> {
        if request_id.len() != 64 || !request_id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(OAuthFail::InvalidRequest("unknown authorization".into()));
        }
        self.write(|tx| {
            let Some(req) = load_request(tx, request_id)? else {
                return Err(Error::NotFound("unknown authorization".into()));
            };
            let status = if req.status == "pending" && req.expires_at <= tx.now {
                tx.execute(
                    "UPDATE oauth_requests SET status = 'expired' WHERE request_id = ?1",
                    [&req.request_id],
                )?;
                "expired".to_string()
            } else {
                req.status.clone()
            };
            let redirect = auth_redirect(&req, &status, tx.now, issuer);
            Ok(AuthorizationStatus { status, redirect })
        })
        .map_err(|e| match e {
            Error::NotFound(m) => OAuthFail::InvalidRequest(m),
            other => OAuthFail::from(other),
        })
    }

    pub fn exchange_code(
        &self,
        exchange: CodeExchange,
    ) -> std::result::Result<IssuedTokens, OAuthFail> {
        let Some((request_id, _)) = split_prefixed(&exchange.code, CODE_PREFIX) else {
            return Err(OAuthFail::InvalidGrant(
                "invalid, expired or already used authorization code".into(),
            ));
        };
        let request_id = request_id.to_string();
        let resource = canonical_resource(&exchange.resource).map_err(OAuthFail::InvalidTarget)?;
        let challenge = pkce_s256(&exchange.code_verifier)?;
        let remote = exchange.remote.clone();
        self.write(|tx| {
            let Some(req) = load_request(tx, &request_id)? else {
                return Err(Error::Forbidden(
                    "invalid, expired or already used authorization code".into(),
                ));
            };
            let hash_ok = req
                .code_hash
                .as_deref()
                .is_some_and(|h| secrets_eq(h, &hash_secret(&exchange.code)));
            // A wrong secret must not revoke the grant. Only a second presentation of the
            // real code does (the hash still matches after the code column is cleared).
            if !hash_ok {
                return Err(Error::Forbidden(
                    "invalid, expired or already used authorization code".into(),
                ));
            }
            if req.status == "consumed" {
                // The revocation has to commit, so it cannot share a transaction that then
                // returns an error (that rolls the revoke back).
                return Ok(Err(req.grant_id.clone()));
            }
            let fresh = req.code_expires_at.is_some_and(|t| t > tx.now);
            if req.status != "approved"
                || !fresh
                || req.client_id != exchange.client_id
                || req.redirect_uri != exchange.redirect_uri
                || req.code_challenge != challenge
                || req.code_challenge_method != "S256"
                || canonical_resource(&req.resource).ok().as_deref() != Some(resource.as_str())
            {
                return Err(Error::Forbidden(
                    "invalid, expired or already used authorization code".into(),
                ));
            }
            let grant_id = req.grant_id.clone().ok_or_else(|| {
                Error::Forbidden("invalid, expired or already used authorization code".into())
            })?;
            let grant_revoked: Option<Option<i64>> = tx
                .query_row(
                    "SELECT revoked_at FROM oauth_grants WHERE grant_id = ?1",
                    [&grant_id],
                    |r| r.get(0),
                )
                .optional()?;
            if grant_revoked.flatten().is_some() || grant_revoked.is_none() {
                return Err(Error::Forbidden(
                    "invalid, expired or already used authorization code".into(),
                ));
            }
            tx.execute(
                "UPDATE oauth_requests SET status = 'consumed', code = NULL WHERE request_id = ?1",
                [&request_id],
            )?;
            let issued = issue_pair(tx, &grant_id, &resource, &req.scope)?;
            insert_audit(
                tx,
                AuditRecord {
                    token_id: Some(grant_id),
                    agent: Some(issued.agent.clone()),
                    operation: "oauth.token".into(),
                    outcome: "ok".into(),
                    detail: Some(format!("client {}", req.client_name)),
                    remote,
                },
            )?;
            Ok(Ok(issued))
        })
        .map_err(|e| match e {
            Error::Forbidden(m) => OAuthFail::InvalidGrant(m),
            other => OAuthFail::from(other),
        })?
        .map_err(|grant_id| {
            if let Some(grant_id) = grant_id {
                let _ = self.write(|tx| {
                    revoke_grant_tx(tx, &grant_id)?;
                    insert_audit(
                        tx,
                        AuditRecord {
                            token_id: Some(grant_id),
                            operation: "oauth.token".into(),
                            outcome: "denied".into(),
                            detail: Some("authorization code replay; grant revoked".into()),
                            remote: exchange.remote.clone(),
                            ..Default::default()
                        },
                    )?;
                    Ok(())
                });
            }
            OAuthFail::InvalidGrant("invalid, expired or already used authorization code".into())
        })
    }

    pub fn refresh_oauth(
        &self,
        exchange: RefreshExchange,
    ) -> std::result::Result<IssuedTokens, OAuthFail> {
        let Some((token_id, _)) = split_prefixed(&exchange.refresh_token, REFRESH_PREFIX) else {
            return Err(OAuthFail::InvalidGrant(
                "invalid, expired or revoked refresh token".into(),
            ));
        };
        let token_id = token_id.to_string();
        let resource = canonical_resource(&exchange.resource).map_err(OAuthFail::InvalidTarget)?;
        let presented_hash = hash_secret(&exchange.refresh_token);
        let remote = exchange.remote.clone();
        self.write(|tx| {
            let row = tx
                .query_row(
                    "SELECT grant_id, secret_hash, resource, scope, expires_at, revoked_at, replaced_by
                     FROM oauth_tokens WHERE token_id = ?1 AND kind = 'refresh'",
                    [&token_id],
                    |r| {
                        Ok(RefreshRow {
                            grant_id: r.get(0)?,
                            secret_hash: r.get(1)?,
                            resource: r.get(2)?,
                            scope: r.get(3)?,
                            expires_at: r.get(4)?,
                            revoked_at: r.get(5)?,
                            replaced_by: r.get(6)?,
                        })
                    },
                )
                .optional()?;
            let Some(RefreshRow {
                grant_id,
                secret_hash: stored,
                resource: token_resource,
                scope,
                expires_at,
                revoked_at,
                replaced_by,
            }) = row
            else {
                return Err(Error::Forbidden(
                    "invalid, expired or revoked refresh token".into(),
                ));
            };
            if !secrets_eq(&stored, &presented_hash) {
                return Err(Error::Forbidden(
                    "invalid, expired or revoked refresh token".into(),
                ));
            }
            if replaced_by.is_some() {
                // Revocation commits in a following transaction; returning an error here
                // would roll it back.
                return Ok(Err(grant_id));
            }
            let grant = load_grant(tx, &grant_id)?;
            if revoked_at.is_some()
                || grant.revoked_at.is_some()
                || expires_at <= tx.now
                || grant.client_id != exchange.client_id
                || canonical_resource(&token_resource).ok().as_deref() != Some(resource.as_str())
            {
                return Err(Error::Forbidden(
                    "invalid, expired or revoked refresh token".into(),
                ));
            }
            let issued = issue_pair(tx, &grant_id, &resource, &scope)?;
            let new_id = issued
                .refresh_token
                .as_deref()
                .and_then(|s| split_prefixed(s, REFRESH_PREFIX))
                .map(|(id, _)| id.to_string());
            let changed = tx.execute(
                "UPDATE oauth_tokens SET revoked_at = ?2, replaced_by = ?3 WHERE token_id = ?1",
                params![token_id, tx.now, new_id],
            )?;
            if changed != 1 || new_id.is_none() {
                return Err(Error::Invalid(format!(
                    "refresh rotation did not record the replacement (rows {changed})"
                )));
            }
            insert_audit(
                tx,
                AuditRecord {
                    token_id: Some(grant_id),
                    agent: Some(issued.agent.clone()),
                    operation: "oauth.refresh".into(),
                    outcome: "ok".into(),
                    detail: Some("rotated".into()),
                    remote,
                },
            )?;
            Ok(Ok(issued))
        })
        .map_err(|e| match e {
            Error::Forbidden(m) | Error::NotFound(m) | Error::Invalid(m) => {
                OAuthFail::InvalidGrant(m)
            }
            other => OAuthFail::from(other),
        })?
        .map_err(|grant_id| {
            let _ = self.write(|tx| {
                revoke_grant_tx(tx, &grant_id)?;
                insert_audit(
                    tx,
                    AuditRecord {
                        token_id: Some(grant_id.clone()),
                        operation: "oauth.refresh".into(),
                        outcome: "denied".into(),
                        detail: Some("refresh token reuse; grant revoked".into()),
                        remote: exchange.remote.clone(),
                        ..Default::default()
                    },
                )?;
                Ok(())
            });
            OAuthFail::InvalidGrant("invalid, expired or revoked refresh token".into())
        })
    }

    /// Revoke the grant that owns this access or refresh token. Unknown tokens are a no-op.
    pub fn revoke_oauth_token(&self, token: &str, remote: Option<String>) -> Result<bool> {
        let parsed = split_prefixed(token, ACCESS_PREFIX)
            .map(|(id, _)| ("access", id.to_string()))
            .or_else(|| {
                split_prefixed(token, REFRESH_PREFIX).map(|(id, _)| ("refresh", id.to_string()))
            });
        let Some((kind, token_id)) = parsed else {
            return Ok(false);
        };
        let digest = hash_secret(token);
        self.write(|tx| {
            let row: Option<(String, String)> = tx
                .query_row(
                    "SELECT grant_id, secret_hash FROM oauth_tokens
                     WHERE token_id = ?1 AND kind = ?2",
                    params![token_id, kind],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((grant_id, stored)) = row else {
                return Ok(false);
            };
            if !secrets_eq(&stored, &digest) {
                return Ok(false);
            }
            let grant = load_grant(tx, &grant_id).ok();
            revoke_grant_tx(tx, &grant_id)?;
            insert_audit(
                tx,
                AuditRecord {
                    token_id: Some(grant_id),
                    agent: grant.as_ref().map(|g| g.agent.clone()),
                    operation: "oauth.revoke".into(),
                    outcome: "ok".into(),
                    detail: Some(format!("client token kind {kind}")),
                    remote,
                },
            )?;
            Ok(true)
        })
    }

    /// Check an OAuth access token. `expected_resource` is this server's canonical MCP URI.
    pub fn verify_access_token(&self, bearer: &str, expected_resource: &str) -> Result<TokenScope> {
        let denied = || Error::Forbidden("invalid or revoked token".into());
        let Some((token_id, _)) = split_prefixed(bearer, ACCESS_PREFIX) else {
            return Err(denied());
        };
        let token_id = token_id.to_string();
        let digest = hash_secret(bearer);
        let expected = canonical_resource(expected_resource).map_err(|_| denied())?;
        self.write(|tx| {
            let row: Option<(String, String, String, i64, Option<i64>)> = tx
                .query_row(
                    "SELECT grant_id, secret_hash, resource, expires_at, revoked_at
                     FROM oauth_tokens WHERE token_id = ?1 AND kind = 'access'",
                    [&token_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()?;
            let Some((grant_id, stored, resource, expires_at, revoked_at)) = row else {
                return Err(denied());
            };
            let resource_ok =
                canonical_resource(&resource).ok().as_deref() == Some(expected.as_str());
            if !secrets_eq(&stored, &digest)
                || revoked_at.is_some()
                || expires_at <= tx.now
                || !resource_ok
            {
                return Err(denied());
            }
            let grant = load_grant(tx, &grant_id)?;
            if grant.revoked_at.is_some() {
                return Err(denied());
            }
            tx.execute(
                "UPDATE oauth_tokens SET last_used_at = ?2 WHERE token_id = ?1",
                params![token_id, tx.now],
            )?;
            tx.execute(
                "UPDATE oauth_grants SET last_used_at = ?2 WHERE grant_id = ?1",
                params![grant_id, tx.now],
            )?;
            let agent_id: String = tx.query_row(
                "SELECT agent_id FROM oauth_grants WHERE grant_id = ?1",
                [&grant_id],
                |r| r.get(0),
            )?;
            let agent = crate::identity::get_agent(tx, &agent_id)?;
            Ok(TokenScope {
                token_id: grant_id,
                agent_id: agent.agent_id,
                agent: agent.address,
                project_globs: grant.project_globs,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_rules() {
        let registered = vec![
            "https://claude.ai/api/mcp/auth_callback".into(),
            "http://127.0.0.1/callback".into(),
            "http://localhost/callback".into(),
        ];
        assert!(redirect_matches(
            &registered,
            "https://claude.ai/api/mcp/auth_callback"
        ));
        assert!(redirect_matches(
            &registered,
            "http://127.0.0.1:3118/callback"
        ));
        assert!(redirect_matches(
            &registered,
            "http://localhost:3118/callback"
        ));
        assert!(!redirect_matches(
            &registered,
            "http://127.0.0.1:3118/other"
        ));
        assert!(!redirect_matches(
            &["http://localhost/callback".into()],
            "http://127.0.0.1:3118/callback"
        ));
        assert!(!redirect_matches(
            &registered,
            "https://evil.example/api/mcp/auth_callback"
        ));
        assert!(!redirect_matches(
            &registered,
            "http://127.0.0.1:1/callback#x"
        ));
        assert!(validate_redirect_uri(
            "https://chatgpt.com/connector_platform_oauth_redirect",
            "web"
        )
        .is_ok());
        assert!(validate_redirect_uri("http://127.0.0.1/callback", "web").is_err());
        assert!(validate_redirect_uri("http://127.0.0.1/callback", "native").is_ok());
        assert!(validate_redirect_uri("http://evil.example/callback", "native").is_err());
    }

    #[test]
    fn resource_and_cimd() {
        assert_eq!(
            canonical_resource("HTTPS://MCP.Example.com:443/mcp/").unwrap(),
            "https://mcp.example.com/mcp"
        );
        assert!(canonical_resource("https://mcp.example.com/mcp?x=1").is_err());
        assert!(canonical_resource("mcp.example.com").is_err());
        let url = "https://chatgpt.com/oauth/client.json";
        let doc = serde_json::json!({
            "client_id": url,
            "client_name": "ChatGPT",
            "redirect_uris": ["https://chatgpt.com/connector_platform_oauth_redirect"],
            "token_endpoint_auth_methods_supported": ["none", "private_key_jwt"],
            "token_endpoint_auth_method": "private_key_jwt"
        });
        let profile = validate_cimd(url, &doc).unwrap();
        assert_eq!(profile.client_name, "ChatGPT");
        assert!(validate_cimd("https://other.example/client.json", &doc).is_err());
        let mut bad = doc.clone();
        bad["token_endpoint_auth_method"] = serde_json::json!("private_key_jwt");
        bad.as_object_mut()
            .unwrap()
            .remove("token_endpoint_auth_methods_supported");
        assert!(validate_cimd(url, &bad).is_err());
    }

    #[test]
    fn grant_is_one_agent_and_refresh_reuse_revokes_it() {
        use crate::identity::{Program, RegisterAgent};
        use crate::store::Clock;

        let (clock, now) = Clock::manual(1_700_000_000_000);
        let store = Store::open_in_memory(clock).unwrap();
        store
            .register_agent(RegisterAgent {
                name: "chatgpt".into(),
                program: Program::Other,
                project: None,
                model: None,
                description: None,
            })
            .unwrap();
        let client = store
            .register_oauth_client(ClientRegistration {
                client_name: "ChatGPT".into(),
                redirect_uris: vec!["https://chatgpt.com/connector_platform_oauth_redirect".into()],
                application_type: Some("web".into()),
                remote: None,
            })
            .unwrap();
        let pending = store
            .begin_authorization(AuthorizeInput {
                client: ClientProfile {
                    client_id: client.client_id.clone(),
                    client_name: client.client_name.clone(),
                    redirect_uris: client.redirect_uris.clone(),
                },
                redirect_uri: client.redirect_uris[0].clone(),
                state: Some("xyz".into()),
                scope: Some("another offline_access".into()),
                resource: "https://another.example.com/mcp".into(),
                code_challenge: pkce_s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk").unwrap(),
                code_challenge_method: "S256".into(),
                remote: None,
            })
            .unwrap();
        let grant = store
            .approve_oauth(Approval {
                user_code: pending.user_code,
                agent: "chatgpt".into(),
                project_globs: vec!["/work/*".into()],
                label: Some("chat".into()),
            })
            .unwrap();
        assert_eq!(grant.agent, "chatgpt");
        assert_eq!(grant.project_globs, vec!["/work/*".to_string()]);
        let status = store
            .authorization_status(&pending.request_id, "https://another.example.com")
            .unwrap();
        let redirect = status.redirect.unwrap();
        assert!(redirect.contains("iss=https%3A%2F%2Fanother.example.com"));
        assert!(redirect.contains("state=xyz"));
        let code = redirect
            .split("code=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap()
            .to_string();
        // A guess that only knows the request id must not revoke the grant.
        let forged = format!("anoac_{}_{}", pending.request_id, "0".repeat(64));
        let forged = store.exchange_code(CodeExchange {
            code: forged,
            client_id: client.client_id.clone(),
            redirect_uri: client.redirect_uris[0].clone(),
            code_verifier: "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into(),
            resource: "https://another.example.com/mcp".into(),
            remote: None,
        });
        assert!(forged.is_err());
        let issued = store
            .exchange_code(CodeExchange {
                code: code.clone(),
                client_id: client.client_id.clone(),
                redirect_uri: client.redirect_uris[0].clone(),
                code_verifier: "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into(),
                resource: "https://another.example.com/mcp".into(),
                remote: None,
            })
            .unwrap();
        assert!(issued.access_token.starts_with("anoat_"));
        assert!(issued.refresh_token.is_some());
        let scope = store
            .verify_access_token(&issued.access_token, "https://another.example.com/mcp")
            .unwrap();
        assert_eq!(scope.agent, "chatgpt");
        assert_eq!(scope.project_globs, vec!["/work/*".to_string()]);
        now.fetch_add(3_600_000 + 1, std::sync::atomic::Ordering::SeqCst);
        assert!(store
            .verify_access_token(&issued.access_token, "https://another.example.com/mcp")
            .is_err());
        let old_refresh = issued.refresh_token.clone().unwrap();
        let parsed = old_refresh
            .strip_prefix("anort_")
            .and_then(|r| r.split_once('_'))
            .map(|(id, hex)| {
                (
                    id.len(),
                    hex.len(),
                    hex.bytes().all(|b| b.is_ascii_hexdigit()),
                )
            });
        assert_eq!(parsed, Some((26, 64, true)), "token was {old_refresh}");
        let refreshed = store
            .refresh_oauth(RefreshExchange {
                refresh_token: old_refresh.clone(),
                client_id: client.client_id.clone(),
                resource: "https://another.example.com/mcp".into(),
                remote: None,
            })
            .unwrap();
        let new_refresh = refreshed.refresh_token.clone().unwrap();
        let parsed_new = new_refresh
            .strip_prefix("anort_")
            .and_then(|r| r.split_once('_'))
            .map(|(id, hex)| (id.len(), hex.len()));
        assert_eq!(parsed_new, Some((26, 64)), "new token {new_refresh}");
        assert!(store
            .verify_access_token(&refreshed.access_token, "https://another.example.com/mcp")
            .is_ok());
        // Reuse of the replaced refresh token revokes the grant.
        assert!(store
            .refresh_oauth(RefreshExchange {
                refresh_token: old_refresh,
                client_id: client.client_id.clone(),
                resource: "https://another.example.com/mcp".into(),
                remote: None,
            })
            .is_err());
        assert!(store
            .refresh_oauth(RefreshExchange {
                refresh_token: new_refresh,
                client_id: client.client_id,
                resource: "https://another.example.com/mcp".into(),
                remote: None,
            })
            .is_err());
        let audit = store.audit_log(50).unwrap();
        let blob = format!("{audit:?}");
        assert!(!blob.contains("anoat_"));
        assert!(!blob.contains("anort_"));
        assert!(!blob.contains("anoac_"));
        assert!(!blob.contains("dBjftJeZ4CVP"));
    }

    #[test]
    fn pkce_known_answer() {
        // RFC 7636 appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_s256(verifier).unwrap(),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
