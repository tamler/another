//! `another doctor`: one read-only look at an installation, with the command that fixes
//! each problem (docs/DOCTOR.md).
//!
//! Nothing here creates the home, migrates or writes the database, edits a hook config or
//! touches a service. The only network call is one `GET /health` to a loopback address.

use std::path::{Path, PathBuf};
use std::time::Duration;

use another_core::{Error, Store};
use serde::Serialize;

use crate::hooks::{self, Client, HookEnv};

/// How long the database may stay locked, and how long `/health` may take.
const WAIT: Duration = Duration::from_secs(2);
/// A hub not synced for this long is worth a warning.
const HUB_STALE_MS: i64 = 24 * 60 * 60 * 1000;
/// A spoke not heard from for this long is worth a warning.
const SPOKE_STALE_MS: i64 = 7 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Ok,
    Info,
    Warn,
    Fail,
    Skip,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Fail => "fail",
            Level::Skip => "skip",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub status: Level,
    pub detail: String,
    pub fix: Option<String>,
}

fn check(name: &'static str, status: Level, detail: impl Into<String>) -> Check {
    Check {
        name,
        status,
        detail: detail.into(),
        fix: None,
    }
}

impl Check {
    fn fix(mut self, fix: impl Into<String>) -> Check {
        self.fix = Some(fix.into());
        self
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// No check failed. Warnings do not count.
    pub ok: bool,
    pub checks: Vec<Check>,
}

/// Where to look.
pub struct Inputs {
    pub another_home: PathBuf,
    /// Loopback address for `/health`; `None` means the daemon's bind, else the default.
    pub bind: Option<String>,
}

pub fn run(inputs: &Inputs) -> Report {
    let mut checks = vec![binary()];
    let home = home(&inputs.another_home);
    let home_ok = home.status != Level::Fail;
    checks.push(home);

    let mut store = None;
    if home_ok {
        let (db, opened) = database(&inputs.another_home);
        checks.push(db);
        store = opened;
    } else {
        checks.push(check("database", Level::Skip, "no home directory"));
    }
    checks.push(hook_check(&inputs.another_home));

    // launchd and the plist only; the one `/health` call is the server check's, and the
    // daemon row reuses its answer when both name the same address.
    let daemon = daemon_status(&inputs.another_home);
    let bind = inputs
        .bind
        .clone()
        .or_else(|| daemon.as_ref().ok().and_then(|d| d.bind.clone()))
        .unwrap_or_else(|| crate::serve::DEFAULT_BIND.to_string());
    let (server_row, answered) = server(&bind, store.as_ref().map(|s| s.node_id()));
    checks.push(server_row);
    let health = answered.then_some(bind.as_str());
    checks.push(daemon_check(daemon, &inputs.another_home, health));
    checks.push(match &store {
        Some(store) => peers(store),
        None => check(
            "peers",
            Level::Skip,
            "not checked until the database check passes",
        ),
    });

    Report {
        ok: checks.iter().all(|c| c.status != Level::Fail),
        checks,
    }
}

/// This binary, and whether it is the `another` the shell finds first.
fn binary() -> Check {
    let version = env!("CARGO_PKG_VERSION");
    let Ok(me) = std::env::current_exe() else {
        return check(
            "binary",
            Level::Warn,
            format!("another {version}; its own path is unknown"),
        );
    };
    let file = if cfg!(windows) {
        "another.exe"
    } else {
        "another"
    };
    let on_path = std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .filter(|d| d.is_absolute())
            .map(|d| d.join(file))
            .find(|c| c.is_file())
    });
    match on_path {
        Some(found) if same_file(&found, &me) => check(
            "binary",
            Level::Ok,
            format!("another {version} at {} (the one on PATH)", me.display()),
        ),
        Some(found) => check(
            "binary",
            Level::Warn,
            format!(
                "another {version} at {}, but PATH finds {} first",
                me.display(),
                found.display()
            ),
        )
        .fix("put this binary's directory first on PATH, or reinstall so both are the same"),
        None => check(
            "binary",
            Level::Warn,
            format!("another {version} at {}; no another on PATH", me.display()),
        )
        .fix(format!(
            "add {} to PATH; hooks and the daemon call another by path",
            me.parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        )),
    }
}

fn home(path: &Path) -> Check {
    let shown = path.display().to_string();
    match std::fs::metadata(path) {
        Err(_) => check("home", Level::Fail, format!("{shown} does not exist")).fix("another init"),
        Ok(m) if !m.is_dir() => check("home", Level::Fail, format!("{shown} is not a directory"))
            .fix(format!("move {shown} away, then another init")),
        Ok(m) => mode_check(&shown, &m),
    }
}

#[cfg(unix)]
fn mode_check(shown: &str, m: &std::fs::Metadata) -> Check {
    use std::os::unix::fs::PermissionsExt;
    let mode = m.permissions().mode() & 0o777;
    if mode == 0o700 {
        check("home", Level::Ok, format!("{shown} (0700)"))
    } else {
        check(
            "home",
            Level::Warn,
            format!("{shown} is {mode:o}; other users may read it"),
        )
        .fix(format!("chmod 700 {shown}"))
    }
}

#[cfg(not(unix))]
fn mode_check(shown: &str, _m: &std::fs::Metadata) -> Check {
    check("home", Level::Ok, shown.to_string())
}

/// The database check, and the store opened for later checks when the schema is current.
fn database(home: &Path) -> (Check, Option<Store>) {
    let inspected = match Store::inspect(home, WAIT) {
        Ok(i) => i,
        Err(Error::NotFound(_)) => {
            return (
                check("database", Level::Fail, "no database in the home directory")
                    .fix("another init"),
                None,
            )
        }
        Err(Error::Db(e)) if e.to_string().contains("locked") || e.to_string().contains("busy") => {
            return (
                check("database", Level::Warn, "busy: another process held it for 2 s")
                    .fix("run another doctor again"),
                None,
            )
        }
        Err(e) => {
            return (
                check("database", Level::Fail, format!("could not read it: {e}"))
                    .fix("stop another serve and the daemon, copy another.db together with its -wal and -shm files, and see docs/DOCTOR.md"),
                None,
            )
        }
    };
    if inspected.quick_check != "ok" {
        return (
            check(
                "database",
                Level::Fail,
                format!("SQLite quick_check: {}", inspected.quick_check),
            )
            .fix("stop another serve and the daemon, copy another.db together with its -wal and -shm files, and see docs/DOCTOR.md"),
            None,
        );
    }
    let (v, want) = (inspected.version, inspected.expected);
    if v > want {
        return (
            check(
                "database",
                Level::Fail,
                format!("schema v{v} is newer than this build (v{want})"),
            )
            .fix("upgrade another; an older build cannot use this database"),
            None,
        );
    }
    if v < want {
        return (
            check(
                "database",
                Level::Warn,
                format!("schema v{v}, this build migrates it to v{want}"),
            )
            .fix("another status (any normal command migrates it)"),
            None,
        );
    }
    match Store::open_read_only(home, WAIT) {
        Ok(store) => {
            let node = store
                .node()
                .map(|n| n.display_name)
                .unwrap_or_else(|_| "unknown".into());
            (
                check(
                    "database",
                    Level::Ok,
                    format!("schema v{v} (current), quick_check ok, node {node}"),
                ),
                Some(store),
            )
        }
        Err(e) => (
            check("database", Level::Fail, format!("no local node: {e}")).fix("another init"),
            None,
        ),
    }
}

fn hook_check(another_home: &Path) -> Check {
    let Ok(env) = HookEnv::detect(another_home.to_path_buf()) else {
        return check("hooks", Level::Skip, "HOME is not set");
    };
    // Each installed client config names the wrapper it runs, by absolute path.
    let mut installed: Vec<(Client, Vec<PathBuf>)> = Vec::new();
    for c in Client::ALL {
        if let Ok(text) = std::fs::read_to_string(c.config_path(&env.home)) {
            if text.contains(hooks::MARKER) {
                installed.push((c, wrapper_paths(&text)));
            }
        }
    }
    let clients: Vec<Client> = installed.iter().map(|(c, _)| *c).collect();
    let missing: Vec<&str> = Client::ALL
        .into_iter()
        .filter(|c| !clients.contains(c))
        .map(|c| c.as_str())
        .collect();
    if installed.is_empty() {
        return check("hooks", Level::Info, "not installed for any client")
            .fix("another hook install --client claude (or codex, grok, cursor, all)");
    }
    let ours = env.script_path();
    for (client, paths) in &installed {
        if let Some(broken) = paths.iter().find(|p| !runnable(p)) {
            return check(
                "hooks",
                Level::Warn,
                format!(
                    "{} runs {}, which is missing or not executable",
                    client.as_str(),
                    broken.display()
                ),
            )
            .fix(format!("another hook install --client {}", client.as_str()));
        }
        if let Some(other) = paths.iter().find(|p| !same_file(p, &ours)) {
            return check(
                "hooks",
                Level::Warn,
                format!(
                    "{} runs {}, which belongs to another home than {}",
                    client.as_str(),
                    other.display(),
                    another_home.display()
                ),
            )
            .fix(format!(
                "with ANOTHER_HOME set to the home you use: another hook install --client {}",
                client.as_str()
            ));
        }
    }
    let detail = if missing.is_empty() {
        format!("installed for {}", names(&clients))
    } else {
        format!(
            "installed for {}; not for {}",
            names(&clients),
            missing.join(", ")
        )
    };
    check("hooks", Level::Ok, detail)
}

/// The wrapper paths a client config runs. The installer writes each command as the
/// single-quoted absolute path of `another-hook.sh` followed by its arguments.
fn wrapper_paths(config: &str) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    for (at, _) in config.match_indices(hooks::MARKER) {
        let end = at + hooks::MARKER.len();
        let Some(start) = config[..at].rfind(['\'', '"']) else {
            continue;
        };
        let path = PathBuf::from(&config[start + 1..end]);
        if path.is_absolute() && !found.contains(&path) {
            found.push(path);
        }
    }
    found
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn names(cs: &[Client]) -> String {
    cs.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(", ")
}

#[cfg(unix)]
fn runnable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn runnable(p: &Path) -> bool {
    p.is_file()
}

/// Enough for `/health`'s small JSON object.
const HEALTH_BODY_MAX: u64 = 16 * 1024;

/// The one network call: `GET /health` on a numeric loopback address, with no proxy and
/// no redirect followed. Returns the row and whether this home's server answered 2xx: a
/// server that names another node (`local_node` is this database's) is a warning.
fn server(bind: &str, local_node: Option<&str>) -> (Check, bool) {
    // A literal IP only: no name lookup, so no name can point it anywhere else.
    let loopback = bind
        .parse::<std::net::SocketAddr>()
        .is_ok_and(|a| a.ip().is_loopback());
    if !loopback {
        return (
            check(
                "server",
                Level::Skip,
                format!(
                    "{bind} is not a numeric loopback address (127.0.0.1:PORT or [::1]:PORT); \
                     doctor only contacts loopback"
                ),
            ),
            false,
        );
    }
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(WAIT))
            .build(),
    );
    match agent.get(&format!("http://{bind}/health")).call() {
        Ok(mut r) if r.status().is_success() => {
            // `another serve` names its node. A server for another home (another node) is
            // not this installation's: MCP clients would write where the hooks do not read.
            let theirs = r
                .body_mut()
                .with_config()
                .limit(HEALTH_BODY_MAX)
                .read_to_string()
                .ok()
                .and_then(|b| serde_json::from_str::<serde_json::Value>(&b).ok())
                .and_then(|v| v["node_id"].as_str().map(str::to_string));
            match (theirs, local_node) {
                (Some(theirs), Some(ours)) if theirs != ours => (
                    check(
                        "server",
                        Level::Warn,
                        format!(
                            "{bind} serves node {}, not this home's node {ours}",
                            another_core::single_line(&theirs, 64)
                        ),
                    )
                    .fix(
                        "another daemon status, and check which ANOTHER_HOME the server on that \
                         port was started with; pass --bind for this home's server",
                    ),
                    false,
                ),
                _ => (
                    check("server", Level::Ok, format!("answering on {bind}")),
                    true,
                ),
            }
        }
        Ok(r) => (
            check(
                "server",
                Level::Warn,
                format!("{bind} answered /health with {}", r.status().as_u16()),
            )
            .fix(
                "another daemon restart, or restart another serve; is something else on that port?",
            ),
            false,
        ),
        Err(_) => (
            check(
                "server",
                Level::Info,
                format!("nothing answering on {bind}"),
            )
            .fix("another daemon install (macOS) or run another serve"),
            false,
        ),
    }
}

fn daemon_status(another_home: &Path) -> anyhow::Result<crate::daemon::DaemonStatus> {
    if !cfg!(target_os = "macos") {
        anyhow::bail!("not macOS");
    }
    let env = crate::daemon::DaemonEnv::detect(
        another_home.to_path_buf(),
        crate::daemon::DEFAULT_LABEL.into(),
        crate::serve::DEFAULT_BIND.into(),
    )?;
    crate::daemon::service(&env)
}

/// The `ANOTHER_HOME` a launchd plist written by `another daemon install` serves.
fn plist_home(plist: &str) -> Option<PathBuf> {
    let after = plist.split("<key>ANOTHER_HOME</key>").nth(1)?;
    let value = after.split("<string>").nth(1)?.split("</string>").next()?;
    let value = value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&");
    Some(PathBuf::from(value))
}

/// `answered`: the address the server check found answering `/health`, if any.
fn daemon_check(
    status: anyhow::Result<crate::daemon::DaemonStatus>,
    another_home: &Path,
    answered: Option<&str>,
) -> Check {
    if !cfg!(target_os = "macos") {
        return check("daemon", Level::Skip, "the launchd daemon is macOS only");
    }
    let s = match status {
        Ok(s) => s,
        Err(e) => return check("daemon", Level::Skip, format!("could not ask launchd: {e}")),
    };
    if !s.installed {
        return check("daemon", Level::Info, "not installed")
            .fix("another daemon install, if you want another serve running in the background");
    }
    // macOS only (`daemon_status`): `file` is the launchd plist.
    let served = std::fs::read_to_string(&s.file)
        .ok()
        .as_deref()
        .and_then(plist_home);
    if let Some(served) = served.filter(|h| !same_file(h, another_home)) {
        return check(
            "daemon",
            Level::Info,
            format!(
                "{} serves {}, not {}",
                s.label,
                served.display(),
                another_home.display()
            ),
        );
    }
    if !s.loaded {
        return check(
            "daemon",
            Level::Warn,
            format!("{} installed but not loaded", s.label),
        )
        .fix("another daemon restart; see docs/DAEMON.md");
    }
    let pid = s.pid.map(|p| format!(", pid {p}")).unwrap_or_default();
    let Some(bind) = s.bind.as_deref() else {
        return check(
            "daemon",
            Level::Warn,
            format!("{} loaded{pid}, but its plist names no --bind", s.label),
        )
        .fix("another daemon install (rewrites the plist)");
    };
    match answered {
        Some(a) if a == bind => {}
        None => {
            return check(
                "daemon",
                Level::Warn,
                format!(
                    "{} loaded{pid}, but /health on {bind} did not answer",
                    s.label
                ),
            )
            .fix(format!("another daemon restart; see {}", s.log));
        }
        Some(_) => {
            return check(
                "daemon",
                Level::Ok,
                format!(
                    "{} loaded{pid}; its {bind} was not the address checked",
                    s.label
                ),
            )
        }
    }
    check(
        "daemon",
        Level::Ok,
        format!("{} loaded{pid}, healthy", s.label),
    )
}

fn peers(store: &Store) -> Check {
    let peers = match store.peers() {
        Ok(p) => p,
        Err(e) => return check("peers", Level::Warn, format!("could not list peers: {e}")),
    };
    let live: Vec<_> = peers.iter().filter(|p| p.revoked_at.is_none()).collect();
    if live.is_empty() {
        return check("peers", Level::Info, "no paired nodes");
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default();
    let age = |t: &Option<String>| {
        t.as_deref()
            .and_then(another_core::parse_ts)
            .map(|ms| now - ms)
    };
    let mut stale = Vec::new();
    for p in &live {
        let limit = if p.role == "hub" {
            HUB_STALE_MS
        } else {
            SPOKE_STALE_MS
        };
        match age(&p.last_sync_at) {
            Some(a) if a <= limit => {}
            Some(a) => stale.push(format!(
                "{} {}: last sync {} h ago",
                p.role,
                p.display_name,
                a / 3_600_000
            )),
            None => stale.push(format!("{} {}: never synced", p.role, p.display_name)),
        }
    }
    if stale.is_empty() {
        check(
            "peers",
            Level::Ok,
            format!("{} paired, all synced recently", live.len()),
        )
    } else {
        check("peers", Level::Warn, stale.join("; "))
            .fix("another sync, and check the hub URL with another peers")
    }
}

/// Human output: one line per check, and an indented fix under anything not ok.
pub fn render(r: &Report) -> String {
    let mut out = String::new();
    for c in &r.checks {
        out.push_str(&format!(
            "{:<5} {:<9} {}\n",
            c.status.as_str(),
            c.name,
            c.detail
        ));
        if let Some(fix) = &c.fix {
            if c.status != Level::Ok {
                out.push_str(&format!("{:16}fix: {fix}\n", ""));
            }
        }
    }
    out
}
