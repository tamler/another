//! Remote agents (docs/design/07-remote-agents.md): the hub side (`POST /join`, `/agent/*`,
//! `/lanes`) and the client side (`another join`, `another remote …`, the join snippet).

use std::path::{Path, PathBuf};
use std::time::Duration;

use another_core::{
    AuditRecord, HeartbeatReport, JoinClient, Joined, LaneFilter, LaneReportInput, LaneStatus,
    ReportKind, SessionInfo, SessionPhase, Store,
};
use anyhow::{bail, Context, Result};
use axum::extract::{Path as AxPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::http_decisions::finish;
use crate::remote::{audit, RemotePeer};

// ---------------------------------------------------------------------------
// Client configuration

/// The ready-to-paste client configuration for a joined agent.
pub fn client_config(hub: &str, token: &str, agent: &str, lane: Option<&str>) -> Value {
    let hub = hub.trim_end_matches('/');
    let mcp_url = format!("{hub}/mcp");
    let header = format!("Authorization: Bearer {token}");
    let mut env = serde_json::Map::new();
    env.insert("ANOTHER_HUB".into(), json!(hub));
    env.insert("ANOTHER_TOKEN".into(), json!(token));
    env.insert("ANOTHER_AGENT".into(), json!(agent));
    if let Some(l) = lane {
        env.insert("ANOTHER_LANE".into(), json!(l));
    }
    json!({
        "claude_code": {
            "command": ["claude", "mcp", "add", "--scope", "user", "--transport", "http", "another", mcp_url, "--header", header],
            "mcp_json": { "mcpServers": { "another": {
                "type": "http", "url": mcp_url,
                "headers": { "Authorization": format!("Bearer {token}") }
            } } },
        },
        "codex": {
            "config_toml": format!(
                "[mcp_servers.another]\nurl = \"{mcp_url}\"\nbearer_token_env_var = \"ANOTHER_TOKEN\"\n"
            ),
            "note": "Codex reads the token from ANOTHER_TOKEN: start it with that variable set (source remote.env)."
        },
        "env": Value::Object(env),
    })
}

/// A POSIX shell block a cloud session runs to install `another` and join. Contains only
/// the hub URL and the single-use code. The install is pinned to the hub's own version: the
/// release archive must match its published SHA-256 (a mismatch stops the script), and the
/// source fallback builds that same tag.
pub fn snippet(hub: &str, code: &str, expires_at: &str) -> String {
    snippet_for_version(hub, code, expires_at, env!("CARGO_PKG_VERSION"))
}

fn snippet_for_version(hub: &str, code: &str, expires_at: &str, version: &str) -> String {
    format!(
        r#"# Join Another as a remote agent. The code is single-use and expires {expires_at}.
set -eu
HUB='{hub}'
CODE='{code}'
VERSION='{version}'
if ! command -v another >/dev/null 2>&1; then
  BIN="$HOME/.local/bin"; mkdir -p "$BIN"; export PATH="$BIN:$HOME/.cargo/bin:$PATH"
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) T=x86_64-unknown-linux-musl ;;
    Linux-aarch64|Linux-arm64) T=aarch64-unknown-linux-musl ;;
    Darwin-arm64) T=aarch64-apple-darwin ;;
    Darwin-x86_64) T=x86_64-apple-darwin ;;
    *) T= ;;
  esac
  TMP=$(mktemp -d)
  A="another-$VERSION-$T.tar.gz"
  URL="https://github.com/tamler/another/releases/download/v$VERSION"
  if [ -n "$T" ] && curl -fsSL -o "$TMP/$A" "$URL/$A" && curl -fsSL -o "$TMP/$A.sha256" "$URL/$A.sha256"; then
    # The archive must match the release's published checksum; never install it otherwise.
    # $A.sha256 must be one "<sha256>  $A" line. The digests are compared here rather than
    # with `-c`, because some `sha256sum -c` builds skip a line they can't parse and exit 0.
    # The byte count catches what command substitution drops, such as a NUL.
    if [ "$(wc -l < "$TMP/$A.sha256")" -ne 1 ] || [ "$(wc -c < "$TMP/$A.sha256")" -ne $((67 + ${{#A}})) ] ||
      ! grep -qE '^[0-9a-f]{{64}} [ *]' "$TMP/$A.sha256" || [ "$(cut -c67- "$TMP/$A.sha256")" != "$A" ]; then
      echo "another $VERSION: $A.sha256 is not one \"<sha256>  $A\" line, not installing" >&2; rm -rf "$TMP"; exit 1
    fi
    WANT=$(cut -c1-64 "$TMP/$A.sha256")
    if command -v sha256sum >/dev/null 2>&1; then GOT=$(cd "$TMP" && sha256sum "$A") || GOT=
    elif command -v shasum >/dev/null 2>&1; then GOT=$(cd "$TMP" && shasum -a 256 "$A") || GOT=
    else echo "another $VERSION: need sha256sum or shasum to verify, not installing" >&2; rm -rf "$TMP"; exit 1
    fi
    if [ "${{GOT%% *}}" != "$WANT" ]; then
      echo "another $VERSION: checksum mismatch, not installing" >&2; rm -rf "$TMP"; exit 1
    fi
    # `set -e` does not stop on a failure before `&&`, so each step is checked here.
    if ! tar -xzf "$TMP/$A" -C "$TMP" || ! cp "$TMP/another-$VERSION-$T/another" "$BIN/another"; then
      echo "another $VERSION: could not unpack $A, not installing" >&2; rm -rf "$TMP"; exit 1
    fi
  else
    # No prebuilt archive for this platform or version: build the same tag from source.
    cargo install --locked --git https://github.com/tamler/another --tag "v$VERSION" another
  fi
  rm -rf "$TMP"
fi
another join "$HUB" "$CODE" --write-mcp auto --hello
. "${{ANOTHER_HOME:-$HOME/.another}}/remote.env"
"#
    )
}

// ---------------------------------------------------------------------------
// Hub side

#[derive(Debug, Deserialize)]
pub struct JoinBody {
    pub code: String,
    #[serde(default)]
    pub client: JoinClient,
}

#[derive(Debug, Serialize)]
pub struct JoinResponse {
    #[serde(flatten)]
    pub joined: Joined,
    pub hub: String,
    pub mcp: Value,
}

fn hub_url(headers: &HeaderMap) -> String {
    let h = |k: &str| {
        headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let host = h("x-forwarded-host")
        .or_else(|| h("host"))
        .unwrap_or_else(|| "127.0.0.1".into());
    let loopback =
        host.starts_with("127.") || host.starts_with("localhost") || host.starts_with("[::1]");
    let scheme =
        h("x-forwarded-proto").unwrap_or_else(|| if loopback { "http" } else { "https" }.into());
    format!("{scheme}://{host}")
}

/// Join attempts allowed per client address per window.
pub const JOIN_ATTEMPTS: usize = 10;
const JOIN_WINDOW: Duration = Duration::from_secs(60);

/// Sliding-window limit on `/join` attempts, per node and client address.
fn join_allowed(node: &str, client: &str) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;
    static SEEN: OnceLock<Mutex<HashMap<String, Vec<Instant>>>> = OnceLock::new();
    let mut seen = SEEN
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    seen.retain(|_, v| {
        v.retain(|t| now.duration_since(*t) < JOIN_WINDOW);
        !v.is_empty()
    });
    let hits = seen.entry(format!("{node}|{client}")).or_default();
    if hits.len() >= JOIN_ATTEMPTS {
        return false;
    }
    hits.push(now);
    true
}

/// `POST /join` — redeem an invite code. Unauthenticated (the code is the credential), only
/// served with `--auth`, rate-limited per client address, and every attempt is audited.
pub async fn join_handler(State(store): State<Store>, req: axum::extract::Request) -> Response {
    let headers = req.headers().clone();
    let remote = crate::remote::remote_addr(&headers, &req);
    // The client address without the port, so reconnecting doesn't reset the limit.
    let client = remote
        .as_deref()
        .map(|r| match r.parse::<std::net::SocketAddr>() {
            Ok(peer) => peer.ip().to_string(),
            // A trusted forwarding header's value has no port.
            Err(_) => r.to_string(),
        })
        .unwrap_or_else(|| "unknown".into());
    if !join_allowed(store.node_id(), &client) {
        audit(
            &store,
            AuditRecord {
                token_id: None,
                agent: None,
                operation: "POST /join".into(),
                outcome: "denied".into(),
                detail: Some("rate limited".into()),
                remote,
            },
        )
        .await;
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(axum::http::header::RETRY_AFTER, "60")],
            axum::Json(json!({ "error": { "code": "rate_limited", "message": format!("too many join attempts (at most {JOIN_ATTEMPTS} a minute); wait a minute") } })),
        )
            .into_response();
    }
    let body: JoinBody = match axum::body::to_bytes(req.into_body(), 64 * 1024)
        .await
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
    {
        Some(b) => b,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({ "error": { "code": "invalid", "message": "expected {\"code\": \"anj_…\"}" } })),
            )
                .into_response()
        }
    };
    let invite = another_core::invite_id_of(&body.code);
    let detail = format!(
        "invite={} host={:?} version={:?}",
        invite.as_deref().unwrap_or("unparsable"),
        body.client.hostname,
        body.client.version
    );
    let hub = hub_url(&headers);
    let s = store.clone();
    let result =
        tokio::task::spawn_blocking(move || s.redeem_invite(&body.code, body.client)).await;
    let (status, value, outcome, agent, token_id) = match result {
        Ok(Ok(joined)) => {
            let mcp = client_config(&hub, &joined.token, &joined.agent, joined.lane.as_deref());
            let agent = joined.agent.clone();
            let token_id = joined.token_id.clone();
            (
                StatusCode::OK,
                json!(JoinResponse { joined, hub, mcp }),
                "ok",
                Some(agent),
                Some(token_id),
            )
        }
        Ok(Err(e)) => (
            StatusCode::FORBIDDEN,
            json!({ "error": e.body() }),
            "denied",
            None,
            None,
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": { "code": "internal", "message": e.to_string() } }),
            "error",
            None,
            None,
        ),
    };
    audit(
        &store,
        AuditRecord {
            token_id,
            agent,
            operation: "POST /join".into(),
            outcome: outcome.into(),
            detail: Some(detail),
            remote,
        },
    )
    .await;
    (status, axum::Json(value)).into_response()
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(json!({ "error": { "code": "unauthorized", "message": "this route needs a bearer token (serve with --auth)" } })),
    )
        .into_response()
}

#[derive(Debug, Deserialize, Default)]
pub struct HeartbeatBody {
    /// start | beat (default) | end
    pub phase: Option<String>,
}

/// `POST /agent/heartbeat` — the token's agent's session heartbeat.
pub async fn heartbeat(
    State(store): State<Store>,
    peer: Option<axum::Extension<RemotePeer>>,
    body: Option<axum::Json<HeartbeatBody>>,
) -> Response {
    let Some(peer) = peer.map(|p| p.0) else {
        return unauthorized();
    };
    let phase = body
        .and_then(|b| b.0.phase)
        .unwrap_or_else(|| "beat".into());
    let agent = peer.scope.agent.clone();
    let scope = peer.scope.clone();
    let s = store.clone();
    let detail = format!("phase={phase}");
    let result = tokio::task::spawn_blocking(move || {
        let phase = match phase.as_str() {
            "start" => SessionPhase::Start,
            "end" => SessionPhase::End,
            "beat" => SessionPhase::Beat,
            other => {
                return Err(another_core::Error::Invalid(format!(
                    "unknown phase {other:?}"
                )))
            }
        };
        // Only holds and leases inside the token's projects are renewed or released.
        let r: HeartbeatReport = s.session_scoped(
            &agent,
            phase,
            SessionInfo {
                client: Some("remote".into()),
                ..Default::default()
            },
            Some(&scope),
        )?;
        Ok(json!(r))
    })
    .await;
    finish(
        &store,
        Some(peer),
        "POST /agent/heartbeat".into(),
        detail,
        result,
    )
    .await
}

/// `POST /agent/leave` — leave the hub (ephemeral agents: token revoked, holds released).
pub async fn leave(
    State(store): State<Store>,
    peer: Option<axum::Extension<RemotePeer>>,
) -> Response {
    let Some(peer) = peer.map(|p| p.0) else {
        return unauthorized();
    };
    let agent = peer.scope.agent.clone();
    let scope = peer.scope.clone();
    let s = store.clone();
    let result =
        tokio::task::spawn_blocking(move || Ok(json!(s.leave_scoped(&agent, Some(&scope))?))).await;
    finish(
        &store,
        Some(peer),
        "POST /agent/leave".into(),
        String::new(),
        result,
    )
    .await
}

/// `GET /lanes` — lanes in the token's projects.
pub async fn lanes(
    State(store): State<Store>,
    peer: Option<axum::Extension<RemotePeer>>,
) -> Response {
    let Some(peer) = peer.map(|p| p.0) else {
        return unauthorized();
    };
    let scope = peer.scope.clone();
    let s = store.clone();
    let result = tokio::task::spawn_blocking(move || {
        Ok(json!({ "lanes": s.lanes(LaneFilter::default(), Some(&scope))? }))
    })
    .await;
    finish(
        &store,
        Some(peer),
        "GET /lanes".into(),
        String::new(),
        result,
    )
    .await
}

#[derive(Debug, Deserialize)]
pub struct ReportBody {
    pub summary: String,
    pub status: Option<LaneStatus>,
    pub progress: Option<u8>,
}

/// `POST /lanes/{lane}/report` — a status report as the token's agent.
pub async fn lane_report(
    State(store): State<Store>,
    peer: Option<axum::Extension<RemotePeer>>,
    AxPath(lane): AxPath<String>,
    axum::Json(body): axum::Json<ReportBody>,
) -> Response {
    let Some(peer) = peer.map(|p| p.0) else {
        return unauthorized();
    };
    let scope = peer.scope.clone();
    let detail = format!(
        "lane={lane} status={:?} summary=<{} bytes>",
        body.status,
        body.summary.len()
    );
    let s = store.clone();
    let result = tokio::task::spawn_blocking(move || {
        let r = s.report_lane(
            LaneReportInput {
                agent: scope.agent.clone(),
                lane,
                kind: ReportKind::Status,
                summary: body.summary,
                progress: body.progress,
                status: body.status,
                links: vec![],
                artifacts: vec![],
                outcome: None,
                pr_url: None,
            },
            Some(&scope),
        )?;
        Ok(json!(r))
    })
    .await;
    finish(
        &store,
        Some(peer),
        "POST /lanes/{lane}/report".into(),
        detail,
        result,
    )
    .await
}

// ---------------------------------------------------------------------------
// Client side

/// What `another join` saves (`$ANOTHER_HOME/remote.json`, mode 0600).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub hub: String,
    pub agent: String,
    pub token: String,
    pub token_id: String,
    pub lane: Option<String>,
    pub invite_id: String,
    pub project_globs: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum WriteMcp {
    /// Claude Code if `claude` is installed, Codex if `~/.codex` exists.
    Auto,
    Claude,
    Codex,
    Both,
    None,
}

fn http(url: &str) -> ureq::Agent {
    let loopback = ["http://127.0.0.1", "http://localhost", "http://[::1]"]
        .iter()
        .any(|p| url.starts_with(p));
    let mut c = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(30)));
    if loopback {
        c = c.proxy(None);
    }
    ureq::Agent::new_with_config(c.build())
}

fn read(mut resp: ureq::http::Response<ureq::Body>, what: &str) -> Result<Value> {
    let status = resp.status();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::String(text.clone()));
    if !status.is_success() {
        let msg = v["error"]["message"].as_str().unwrap_or(&text).to_string();
        bail!("{what} failed ({status}): {msg}");
    }
    Ok(v)
}

fn write_private(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write;
    opts.open(path)?.write_all(text.as_bytes())?;
    Ok(())
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn hostname() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
}

fn on_path(bin: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(bin))
            .find(|p| p.is_file())
    })
}

/// Redeem an invite on `hub`, save the profile, write MCP config, optionally say hello.
pub fn join(
    home: &Path,
    user_home: &Path,
    hub: &str,
    code: &str,
    write: WriteMcp,
    hello: bool,
) -> Result<Value> {
    let hub = hub.trim_end_matches('/').to_string();
    if !(hub.starts_with("https://") || hub.starts_with("http://")) {
        bail!("hub url must start with https:// (or http:// for loopback)");
    }
    let resp = http(&hub)
        .post(&format!("{hub}/join"))
        .send_json(json!({ "code": code.trim(), "client": { "hostname": hostname(), "version": env!("CARGO_PKG_VERSION") } }))
        .with_context(|| format!("contacting {hub}"))?;
    let v = read(resp, "join")?;
    let profile = Profile {
        hub: hub.clone(),
        agent: v["agent"].as_str().unwrap_or_default().into(),
        token: v["token"].as_str().unwrap_or_default().into(),
        token_id: v["token_id"].as_str().unwrap_or_default().into(),
        lane: v["lane"].as_str().map(str::to_string),
        invite_id: v["invite_id"].as_str().unwrap_or_default().into(),
        project_globs: serde_json::from_value(v["project_globs"].clone()).unwrap_or_default(),
    };
    // The client writes its config from the URL it used (the hub may be behind a proxy).
    let mcp = client_config(
        &hub,
        &profile.token,
        &profile.agent,
        profile.lane.as_deref(),
    );
    write_private(
        &home.join("remote.json"),
        &serde_json::to_string_pretty(&profile)?,
    )?;
    let env: String = mcp["env"]
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| format!("export {k}={}\n", sh_quote(v.as_str().unwrap_or_default())))
                .collect()
        })
        .unwrap_or_default();
    write_private(&home.join("remote.env"), &env)?;

    let mut wrote: Vec<String> = Vec::new();
    let claude_bin = std::env::var_os("ANOTHER_CLAUDE_BIN")
        .map(PathBuf::from)
        .or_else(|| on_path("claude"));
    let want_claude = matches!(write, WriteMcp::Claude | WriteMcp::Both)
        || (write == WriteMcp::Auto && claude_bin.is_some());
    if want_claude {
        let bin =
            claude_bin.context("`claude` is not installed; use the printed config instead")?;
        let args: Vec<String> = mcp["claude_code"]["command"]
            .as_array()
            .map(|a| {
                a.iter()
                    .skip(1)
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let st = std::process::Command::new(&bin).args(&args).status()?;
        if !st.success() {
            bail!("`claude mcp add` failed ({st})");
        }
        wrote.push("claude: user-scope MCP server `another`".into());
    }
    let codex_dir = user_home.join(".codex");
    let want_codex = matches!(write, WriteMcp::Codex | WriteMcp::Both)
        || (write == WriteMcp::Auto && codex_dir.is_dir());
    if want_codex {
        let cfg = codex_dir.join("config.toml");
        let existing = std::fs::read_to_string(&cfg).unwrap_or_default();
        if existing.contains("[mcp_servers.another]") {
            wrote.push(format!(
                "codex: {} already has [mcp_servers.another]; left unchanged",
                cfg.display()
            ));
        } else {
            let mut text = existing;
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push('\n');
            text.push_str(mcp["codex"]["config_toml"].as_str().unwrap_or_default());
            std::fs::create_dir_all(&codex_dir)?;
            std::fs::write(&cfg, text)?;
            wrote.push(format!(
                "codex: added [mcp_servers.another] to {}",
                cfg.display()
            ));
        }
    }
    let mut said_hello = false;
    if hello {
        call(
            &profile,
            "POST",
            "/agent/heartbeat",
            Some(json!({ "phase": "start" })),
        )?;
        if let Some(lane) = &profile.lane {
            call(
                &profile,
                "POST",
                &format!("/lanes/{lane}/report"),
                Some(
                    json!({ "summary": format!("joined from {} (another {})", hostname().unwrap_or_else(|| "a remote host".into()), env!("CARGO_PKG_VERSION")) }),
                ),
            )?;
        }
        said_hello = true;
    }
    Ok(json!({
        "agent": profile.agent,
        "hub": profile.hub,
        "lane": profile.lane,
        "project_globs": profile.project_globs,
        "profile": home.join("remote.json"),
        "env_file": home.join("remote.env"),
        "mcp_written": wrote,
        "hello": said_hello,
        "mcp": mcp,
    }))
}

pub fn load_profile(home: &Path) -> Result<Profile> {
    let text = std::fs::read_to_string(home.join("remote.json"))
        .context("no remote profile; run `another join <hub-url> <code>` first")?;
    Ok(serde_json::from_str(&text)?)
}

/// Call the hub as the profile's agent.
pub fn call(p: &Profile, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
    let url = format!("{}{path}", p.hub);
    let agent = http(&url);
    let auth = format!("Bearer {}", p.token);
    let resp = match (method, body) {
        ("GET", _) => agent.get(&url).header("Authorization", &auth).call(),
        (_, Some(b)) => agent.post(&url).header("Authorization", &auth).send_json(b),
        (_, None) => agent.post(&url).header("Authorization", &auth).send_empty(),
    }
    .with_context(|| format!("contacting {}", p.hub))?;
    read(resp, &format!("{method} {path}"))
}

/// Forget the profile after leaving.
pub fn forget(home: &Path) -> Result<()> {
    for f in ["remote.json", "remote.env"] {
        let _ = std::fs::remove_file(home.join(f));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_snippet_pins_the_hubs_version_and_checksum() {
        let s = super::snippet("https://hub.example", "anj_X_y", "tomorrow");
        let v = env!("CARGO_PKG_VERSION");
        assert!(s.contains(&format!("VERSION='{v}'")), "{s}");
        assert!(!s.contains("releases/latest"), "never an unpinned release");
        assert!(s.contains("releases/download/v$VERSION"));
        assert!(
            s.contains("--tag \"v$VERSION\""),
            "source fallback is pinned too"
        );
        assert!(s.contains("checksum mismatch, not installing"));
        // A checksum failure stops the script instead of falling back.
        let check = s.find("checksum mismatch").unwrap();
        let exit = s[check..].find("exit 1").unwrap();
        let fallback = s[check..].find("cargo install").unwrap();
        assert!(exit < fallback);
    }

    #[cfg(unix)]
    fn target() -> Option<&'static str> {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
            ("linux", "aarch64") => Some("aarch64-unknown-linux-musl"),
            ("macos", "aarch64") => Some("aarch64-apple-darwin"),
            ("macos", "x86_64") => Some("x86_64-apple-darwin"),
            _ => None,
        }
    }

    #[cfg(unix)]
    fn find(tool: &str) -> Option<String> {
        let out = std::process::Command::new("sh")
            .args(["-c", &format!("command -v {tool}")])
            .output()
            .ok()?;
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && path.starts_with('/')).then_some(path)
    }

    #[cfg(unix)]
    fn write_exec(path: &std::path::Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A PATH holding only what the snippet needs, with `checker` as the one checksum tool, a
    /// `curl` that serves files from `rel/` by name, and a `tar` that logs each call to
    /// `tar.log` before running the real one. None when a tool is missing.
    #[cfg(unix)]
    fn tools_path(dir: &std::path::Path, checker: &str) -> Option<std::path::PathBuf> {
        let bin = dir.join("tools");
        std::fs::create_dir_all(&bin).unwrap();
        for tool in [
            "uname", "mkdir", "mktemp", "rm", "cp", "grep", "cut", "wc", "gzip", checker,
        ] {
            std::os::unix::fs::symlink(find(tool)?, bin.join(tool)).unwrap();
        }
        let cp = find("cp")?;
        let tar = find("tar")?;
        // Called as `curl -fsSL -o OUT URL`.
        write_exec(
            &bin.join("curl"),
            &format!(
                "#!/bin/sh\nout=$3; url=$4\nf='{}'/\"${{url##*/}}\"\n[ -f \"$f\" ] || exit 22\nexec '{cp}' \"$f\" \"$out\"\n",
                dir.join("rel").display()
            ),
        );
        write_exec(
            &bin.join("tar"),
            &format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\nexec '{tar}' \"$@\"\n",
                dir.join("tar.log").display()
            ),
        );
        Some(bin)
    }

    /// Writes `rel/` with one release archive holding a stub `another`, and returns the
    /// archive's name and its real `<sha256>  <archive>` line.
    #[cfg(unix)]
    fn release(dir: &std::path::Path, version: &str, target: &str) -> (String, String) {
        let name = format!("another-{version}-{target}");
        let pkg = dir.join("pkg").join(&name);
        std::fs::create_dir_all(&pkg).unwrap();
        write_exec(&pkg.join("another"), "#!/bin/sh\nexit 0\n");
        let rel = dir.join("rel");
        std::fs::create_dir_all(&rel).unwrap();
        let archive = format!("{name}.tar.gz");
        let ok = std::process::Command::new("tar")
            .arg("-czf")
            .arg(rel.join(&archive))
            .arg("-C")
            .arg(dir.join("pkg"))
            .arg(&name)
            .status()
            .unwrap();
        assert!(ok.success());
        let line = checksum_line(&rel, &archive);
        (archive, line)
    }

    /// The `<sha256>  <archive>` line for `rel/<archive>` as it is now.
    #[cfg(unix)]
    fn checksum_line(rel: &std::path::Path, archive: &str) -> String {
        let sum = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "cd '{}' && (sha256sum '{archive}' 2>/dev/null || shasum -a 256 '{archive}')",
                rel.display()
            ))
            .output()
            .unwrap();
        let line = String::from_utf8_lossy(&sum.stdout).to_string();
        assert!(line.len() > 64, "could not checksum {archive}");
        line
    }

    #[cfg(unix)]
    fn run_snippet(
        dir: &std::path::Path,
        version: &str,
        path: &std::path::Path,
    ) -> std::process::Output {
        let home = dir.join("home");
        std::fs::create_dir_all(home.join(".another")).unwrap();
        std::fs::write(home.join(".another/remote.env"), "").unwrap();
        let script = dir.join("join.sh");
        std::fs::write(
            &script,
            super::snippet_for_version("https://hub.example", "anj_X_y", "tomorrow", version),
        )
        .unwrap();
        // Absolute, since the child's PATH has no sh.
        std::process::Command::new("/bin/sh")
            .arg(&script)
            .env_clear()
            .env("PATH", path)
            .env("HOME", &home)
            .output()
            .unwrap()
    }

    /// Some `sha256sum -c` builds (macOS's /sbin/sha256sum) skip a line they can't parse and
    /// still exit 0, so a `.sha256` file that names the archive but is malformed must be
    /// refused, with either checker, before anything is unpacked.
    #[cfg(unix)]
    #[test]
    fn the_snippet_refuses_a_malformed_checksum_line_with_either_checker() {
        let Some(target) = target() else { return };
        let mut ran = 0;
        for checker in ["sha256sum", "shasum"] {
            let dir = tempfile::tempdir().unwrap();
            let Some(path) = tools_path(dir.path(), checker) else {
                eprintln!("skipping {checker}: it or another needed tool is missing");
                continue;
            };
            ran += 1;
            // A `+` and dots in the archive name must be matched literally.
            let version = "0.0.0-test+b.1";
            let (archive, good) = release(dir.path(), version, target);
            let sums = dir.path().join(format!("rel/{archive}.sha256"));
            let installed = dir.path().join("home/.local/bin/another");

            // The restricted PATH is enough to install a good release with this checker.
            std::fs::write(&sums, &good).unwrap();
            let out = run_snippet(dir.path(), version, &path);
            assert!(
                out.status.success(),
                "{checker}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(installed.exists(), "{checker}: not installed");
            std::fs::remove_file(&installed).unwrap();
            std::fs::remove_file(dir.path().join("tar.log")).unwrap();

            let zeros = "0".repeat(64);
            for (what, line) in [
                (
                    "leading space, wrong digest",
                    format!(" {zeros}  {archive}\n"),
                ),
                ("wrong digest", format!("{zeros}  {archive}\n")),
                ("short digest", format!("{}  {archive}\n", "0".repeat(63))),
                ("no separator", format!("{zeros}{archive}\n")),
                ("other archive", format!("{}  other.tar.gz\n", &good[..64])),
                (
                    "dots as wildcards",
                    format!("{}  {}\n", &good[..64], archive.replace('.', "X")),
                ),
                (
                    "NUL in name",
                    format!("{}  {}\0{}\n", &good[..64], &archive[..9], &archive[9..]),
                ),
                ("two lines", format!("{good}{good}")),
                ("empty", String::new()),
            ] {
                std::fs::write(&sums, line).unwrap();
                let out = run_snippet(dir.path(), version, &path);
                let err = String::from_utf8_lossy(&out.stderr);
                assert!(!out.status.success(), "{checker}, {what}: installed: {err}");
                assert!(err.contains("not installing"), "{checker}, {what}: {err}");
                assert!(
                    !dir.path().join("tar.log").exists(),
                    "{checker}, {what}: unpacked before refusing"
                );
                assert!(!installed.exists(), "{checker}, {what}");
            }
        }
        if ran == 0 {
            eprintln!("skipping: needs sha256sum or shasum");
        }
    }

    /// Under `set -eu`, a failing command before `&&` does not stop the script. An archive that
    /// matches its checksum but will not unpack must stop it there, before `another join` can
    /// run some other `another` that the extended PATH finds, such as one in ~/.cargo/bin.
    #[cfg(unix)]
    #[test]
    fn the_snippet_stops_when_the_verified_archive_will_not_unpack() {
        let Some(target) = target() else { return };
        let dir = tempfile::tempdir().unwrap();
        let Some(path) = ["sha256sum", "shasum"]
            .into_iter()
            .find_map(|checker| tools_path(dir.path(), checker))
        else {
            eprintln!("skipping: needs sha256sum or shasum");
            return;
        };
        let version = "0.0.0-test";
        let (archive, _) = release(dir.path(), version, target);
        let rel = dir.path().join("rel");
        std::fs::write(rel.join(&archive), "not a gzip archive\n").unwrap();
        let line = checksum_line(&rel, &archive);
        std::fs::write(rel.join(format!("{archive}.sha256")), line).unwrap();
        // An older `another` that is not on the caller's PATH, so the snippet installs.
        let stale = dir.path().join("home/.cargo/bin");
        std::fs::create_dir_all(&stale).unwrap();
        let ran = dir.path().join("stale-ran");
        write_exec(
            &stale.join("another"),
            &format!("#!/bin/sh\n: > '{}'\n", ran.display()),
        );

        let out = run_snippet(dir.path(), version, &path);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            dir.path().join("tar.log").exists(),
            "never reached tar: {err}"
        );
        assert!(!out.status.success(), "the script succeeded: {err}");
        assert!(err.contains("not installing"), "{err}");
        assert!(!ran.exists(), "ran the stale another: {err}");
        assert!(!dir.path().join("home/.local/bin/another").exists());
    }
}
