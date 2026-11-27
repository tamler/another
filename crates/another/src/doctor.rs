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

    // Service manager and definition only; the one `/health` call is the server check's, and the
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
    checks.push(transports(&inputs.another_home, store.as_ref()));
    checks.extend(orphan_scan(store.as_ref()));

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
    // No build writes a negative version, and none migrates one: this is not an another
    // database, or it was edited by hand.
    if v < 0 {
        return (
            check(
                "database",
                Level::Fail,
                format!("database schema version {v} is not valid"),
            )
            .fix("check that --home or ANOTHER_HOME points at an another home"),
            None,
        );
    }
    let restore = match &inspected.latest_backup {
        Some(backup) => format!(
            "stop every another process and restore {} (docs/DOCTOR.md)",
            backup.display()
        ),
        None => "stop every another process and restore a backup (docs/DOCTOR.md)".to_string(),
    };
    if v > want {
        let by = match &inspected.origin.migrated {
            Some(stamp) if stamp.schema == v => format!(
                "; last migrated by a {} build of another {}{}",
                stamp.build,
                stamp.version,
                stamp
                    .binary
                    .as_ref()
                    .map(|path| format!(" at {path}"))
                    .unwrap_or_default()
            ),
            _ => String::new(),
        };
        return (
            check(
                "database",
                Level::Fail,
                format!("schema v{v} is newer than this build (v{want}){by}"),
            )
            .fix(format!(
                "upgrade another; an older build cannot use this database. Or {restore}"
            )),
            None,
        );
    }
    if !inspected.known {
        return (
            check(
                "database",
                Level::Fail,
                format!(
                    "schema v{v} does not match this build's v{v}: a different build changed it{}",
                    if inspected.differs.is_empty() {
                        String::new()
                    } else {
                        format!(" (tables that differ: {})", inspected.differs.join(", "))
                    }
                ),
            )
            .fix(format!("use the build that wrote it, or {restore}")),
            None,
        );
    }
    // The store's own verdict on a pending migration, so doctor and a normal command agree
    // (also under --dev-migrate, and for an empty database, which is created, not migrated).
    if inspected.blocked && inspected.in_build_directory {
        return (
            check(
                "database",
                Level::Fail,
                format!(
                    "schema v{v}, and this binary runs from a Cargo build directory, which makes it a development build (v{want}); it will not migrate a home that is not a development home"
                ),
            )
            .fix("copy this binary out of the Cargo build directory to a directory on your PATH, or install it with cargo install --locked --path crates/another, and use that copy; to migrate with this binary anyway, pass --dev-migrate (a backup is taken first)"),
            None,
        );
    }
    if inspected.blocked {
        return (
            check(
                "database",
                Level::Fail,
                format!(
                    "schema v{v}, and this development build (v{want}) did not create this home or a release build has used it since, so it will not migrate it"
                ),
            )
            .fix("use the installed another for this home; to migrate it with this build anyway, pass --dev-migrate (a backup is taken first)"),
            None,
        );
    }
    if v == 0 {
        return (
            check(
                "database",
                Level::Warn,
                format!("empty database (schema v0), this build creates schema v{want} in it"),
            )
            .fix("another init"),
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
            .fix("another status (any normal command backs it up, then migrates it)"),
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
            .fix("another daemon install or run another serve"),
            false,
        ),
    }
}

fn daemon_status(another_home: &Path) -> anyhow::Result<crate::daemon::DaemonStatus> {
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        anyhow::bail!("daemon check supports macOS and Linux");
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
    let s = match status {
        Ok(s) => s,
        Err(e) => {
            return check(
                "daemon",
                Level::Skip,
                format!("could not ask service manager: {e}"),
            )
        }
    };
    if !s.installed {
        return check("daemon", Level::Info, "not installed")
            .fix("another daemon install, if you want another serve running in the background");
    }
    let served = std::fs::read_to_string(&s.file)
        .ok()
        .as_deref()
        .and_then(|text| match s.manager {
            crate::daemon::Manager::Launchd => plist_home(text),
            crate::daemon::Manager::Systemd => crate::daemon::systemd_home(text),
            crate::daemon::Manager::TaskScheduler => None,
        });
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
    if let Some(detail) = &s.detail {
        return check("daemon", Level::Warn, detail.clone());
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
            format!(
                "{} loaded{pid}, but its definition names no --bind",
                s.label
            ),
        )
        .fix("another daemon install (rewrites the definition)");
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
    let inconsistent = match store.inconsistent_holds(5) {
        Ok((0, _)) => None,
        Ok((count, sample)) => Some(format!("{count} inconsistent holds: {}; inert since this build; not removed; ends when it expires or its node releases it", sample.join("; "))),
        Err(e) => Some(format!("could not check inconsistent holds: {e}")),
    };
    let peers = match store.peers() {
        Ok(p) => p,
        Err(e) => return check("peers", Level::Warn, format!("could not list peers: {e}")),
    };
    // A pairing row for this node itself: nothing acts on it, so it is not counted as a
    // peer, but it is said to be there.
    let own_row = peers
        .iter()
        .any(|p| p.node_id == store.node_id())
        .then_some(SELF_ROW);
    let live: Vec<_> = peers
        .iter()
        .filter(|p| p.revoked_at.is_none() && p.node_id != store.node_id())
        .collect();
    if live.is_empty() {
        let warnings: Vec<_> = own_row
            .map(str::to_owned)
            .into_iter()
            .chain(inconsistent)
            .collect();
        return if warnings.is_empty() {
            check("peers", Level::Info, "no paired nodes")
        } else {
            check("peers", Level::Warn, warnings.join("; "))
        };
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
    // "Last sync" is the last sync that ran to its end: on a spoke, of this node with its
    // hub; on a hub, the last push or pull of that spoke it answered.
    let mut stale = Vec::new();
    let (mut stale_hub, mut stale_spoke) = (false, false);
    for p in &live {
        let limit = if p.role == "hub" {
            HUB_STALE_MS
        } else {
            SPOKE_STALE_MS
        };
        let line = match age(&p.last_sync_at) {
            Some(a) if a <= limit => continue,
            Some(a) => format!(
                "{} {}: last sync {} h ago",
                p.role,
                p.display_name,
                a / 3_600_000
            ),
            None => format!("{} {}: never synced", p.role, p.display_name),
        };
        stale.push(line);
        if p.role == "hub" {
            stale_hub = true;
        } else {
            stale_spoke = true;
        }
    }
    // What is known about how this node reaches its hub. Neither stops a sync.
    let mut warnings = Vec::new();
    warnings.extend(inconsistent);
    let unchecked = crate::fed::hub_with_unchecked_address(store).map(|p| p.node_id);
    for p in live.iter().filter(|p| p.role == "hub") {
        let Some(url) = p.url.as_deref() else {
            continue;
        };
        let address = crate::fed::shown_address(url);
        if crate::fed::plain_http_off_loopback(url) {
            warnings.push(format!(
                "hub {} is reached over plain http:// at {address}: anyone on the network \
                 path can read mail and the feed in transit",
                p.display_name
            ));
        }
        if unchecked.as_deref() == Some(p.node_id.as_str()) {
            warnings.push(format!(
                "hub {} was joined before invites were signed, so its address {address} was \
                 never checked; compare it with the address the hub's owner gives, or join \
                 again with a fresh invite made with the same --perms",
                p.display_name
            ));
        }
    }
    warnings.extend(own_row.map(str::to_string));
    if stale.is_empty() {
        let synced = format!("{} paired, all synced recently", live.len());
        if warnings.is_empty() {
            return check("peers", Level::Ok, synced);
        }
        return check(
            "peers",
            Level::Warn,
            format!("{synced}; {}", warnings.join("; ")),
        );
    }
    stale.extend(warnings);
    // The fix names what is stale: a hub is synced from here, a spoke from its own node.
    let mut fixes = Vec::new();
    if stale_hub {
        fixes.push("another sync, and check the hub URL with another peers");
    }
    if stale_spoke {
        fixes.push(
            "run another sync on that node; if it is gone for good, another pair revoke <node>",
        );
    }
    check("peers", Level::Warn, stale.join("; ")).fix(fixes.join("; "))
}

/// What `another doctor` says about a pairing row for this node itself.
const SELF_ROW: &str = "a pairing record for this node itself is ignored: an older build \
                        wrote it from an invite that named this node; no action is needed";

/// Whether `program`, as a command transport would start it, can be found: a path as it
/// is, a bare name in the directories of `PATH`.
fn on_path(program: &str) -> bool {
    let found = |p: &Path| {
        runnable(p)
            || (cfg!(windows)
                && ["exe", "cmd", "bat"]
                    .iter()
                    .any(|ext| p.with_extension(ext).is_file()))
    };
    if Path::new(program).components().count() > 1 {
        return found(Path::new(program));
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| found(&dir.join(program))))
}

/// Command transports: whether `transports.json` was accepted, every stored wake target
/// and session row whose transport nothing provides, an old cmux `notify` target, and
/// whether each configured program can be found. Runs none of them.
fn transports(home: &Path, store: Option<&Store>) -> Check {
    let config = crate::transport::load_config(home);
    let mut problems = Vec::new();
    if let Some(refused) = config.refusal() {
        problems.push(refused);
    }
    if let Some(store) = store {
        for t in store.wake_targets().unwrap_or_default() {
            let via = t.via.as_str();
            if !config.provides(via) {
                problems.push(format!(
                    "the wake target of {} uses the transport {via:?}, which is not configured",
                    t.agent
                ));
            } else if crate::wake::is_old_cmux_notify(via, &t.target) {
                problems.push(format!(
                    "the wake target of {} is the cmux target `notify`, which no longer means \
                     a notification (define a transport such as cmux-notify and set it again)",
                    t.agent
                ));
            }
        }
        let mut unprovided: Vec<String> = store
            .transport_sessions()
            .unwrap_or_default()
            .into_iter()
            .filter(|r| !config.provides(&r.transport))
            .map(|r| r.transport)
            .collect();
        unprovided.sort();
        unprovided.dedup();
        for name in unprovided {
            problems.push(format!(
                "session rows use the transport {name:?}, which is not configured"
            ));
        }
    }
    for spec in &config.transports {
        for program in spec.programs() {
            if !on_path(program) {
                problems.push(format!(
                    "{}: the program {program:?} was not found",
                    spec.name
                ));
            }
        }
    }
    if !problems.is_empty() {
        return check("transports", Level::Warn, problems.join("; ")).fix(format!(
            "write or correct {} (docs/WAKE.md, \"Command transports\")",
            home.join(crate::transport::command::CONFIG_FILE).display()
        ));
    }
    if config.transports.is_empty() {
        return check(
            "transports",
            Level::Info,
            "herdr only (no transports.json, so no command transport)",
        );
    }
    let names: Vec<&str> = config.transports.iter().map(|t| t.name.as_str()).collect();
    check(
        "transports",
        Level::Ok,
        format!("herdr and {} from transports.json", names.join(", ")),
    )
}

/// How many lapsed resources or listeners a row names. The count is of all of them.
const NAMED: usize = 5;

/// The orphan scan of `another orphans` (reap.rs, scan.rs), read without its sweep: this
/// node's lapsed resources, and listening processes that nothing registered. It runs
/// `lsof` or `ss`, and `ps`; it writes nothing and stops or signals nothing.
fn orphan_scan(store: Option<&Store>) -> [Check; 2] {
    let Some(store) = store else {
        let skip = |name| {
            check(
                name,
                Level::Skip,
                "not checked until the database check passes",
            )
        };
        return [skip("resources"), skip("listeners")];
    };
    match crate::reap::orphans_unswept(store) {
        Ok(report) => [lapsed_resources(&report), unregistered_listeners(&report)],
        Err(e) => [
            check(
                "resources",
                Level::Warn,
                format!(
                    "could not list resources: {}",
                    another_core::single_line(&e.to_string(), 200)
                ),
            ),
            check(
                "listeners",
                Level::Skip,
                "not scanned: the resources could not be listed",
            ),
        ],
    }
}

/// `seconds` as one rounded-down figure: `40 s`, `12 min`, `3 h`, `9 d`.
fn lasting(seconds: i64) -> String {
    match seconds.max(0) {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min", s / 60),
        s if s < 48 * 3600 => format!("{} h", s / 3600),
        s => format!("{} d", s / 86_400),
    }
}

/// The first `NAMED` of `items`, and how many were left out.
fn named(items: Vec<String>) -> String {
    let more = items.len().saturating_sub(NAMED);
    let mut out = items.into_iter().take(NAMED).collect::<Vec<_>>().join(", ");
    if more > 0 {
        out.push_str(&format!(", and {more} more"));
    }
    out
}

/// This node's registered resources with no live lease (D9). Another node's are that
/// node's to deal with (D25) and are not listed.
fn lapsed_resources(report: &crate::reap::OrphanReport) -> Check {
    let lapsed: Vec<String> = report
        .orphans
        .iter()
        .filter_map(|o| Some((o.resource.as_ref().filter(|r| r.node.is_none())?, o)))
        .map(|(r, o)| {
            format!(
                "{} {} ({})",
                r.kind.as_str(),
                another_core::single_line(&r.name, 60),
                match o.age_seconds {
                    Some(age) => format!("for {}", lasting(age)),
                    None => "not recorded by a sweep yet".to_string(),
                }
            )
        })
        .collect();
    if lapsed.is_empty() {
        return check("resources", Level::Ok, "none lapsed");
    }
    check(
        "resources",
        Level::Warn,
        format!("{} lapsed: {}", lapsed.len(), named(lapsed)),
    )
    .fix("another orphans")
}

/// Listening processes that no registered resource accounts for. Never a warning: one
/// may be a server the owner wants running.
fn unregistered_listeners(report: &crate::reap::OrphanReport) -> Check {
    let Some(scan) = &report.scan else {
        return check("listeners", Level::Skip, "not scanned");
    };
    if let Some(error) = &scan.error {
        return check(
            "listeners",
            Level::Info,
            format!(
                "the listener scan did not run: lsof and ss were tried ({})",
                another_core::single_line(error, 200)
            ),
        );
    }
    let found: Vec<String> = report
        .orphans
        .iter()
        .filter(|o| o.resource.is_none())
        .map(|o| {
            // The program's name only. Its path and arguments are in `another orphans`.
            let program = o
                .command
                .as_deref()
                .and_then(|c| c.split_whitespace().next())
                .and_then(|word| word.rsplit(['/', '\\']).next())
                .unwrap_or("?");
            format!(
                "port {} {}",
                o.port.map_or("?".into(), |p| p.to_string()),
                another_core::single_line(program, 40)
            )
        })
        .collect();
    if found.is_empty() {
        return check(
            "listeners",
            Level::Ok,
            format!("none unregistered ({})", scan.scanner),
        );
    }
    check(
        "listeners",
        Level::Info,
        format!(
            "{} unregistered ({}): {}",
            found.len(),
            scan.scanner,
            named(found)
        ),
    )
    .fix("another orphans")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::{DaemonEnv, DaemonStatus, Manager};

    #[test]
    fn daemon_check_reads_the_home_from_a_systemd_unit_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let env = DaemonEnv {
            manager: Manager::Systemd,
            home: dir.path().join("user"),
            config_home: None,
            another_home: dir.path().join("other home % with \\\"quotes\""),
            exe: PathBuf::from("/work/another"),
            path: None,
            user: None,
            label: "one.toanother.fixture".into(),
            bind: "127.0.0.1:7466".into(),
        };
        let unit = dir.path().join("fixture.service");
        std::fs::write(&unit, crate::daemon::systemd_unit(&env).unwrap()).unwrap();
        let status = DaemonStatus {
            manager: Manager::Systemd,
            label: env.label.clone(),
            file: unit.display().to_string(),
            plist: None,
            installed: true,
            loaded: true,
            detail: None,
            pid: Some(4242),
            bind: Some(env.bind.clone()),
            healthy: false,
            log: "fixture.log".into(),
        };
        let current = dir.path().join("current");
        let row = daemon_check(Ok(status.clone()), &current, Some(&env.bind));
        assert_eq!(row.status, Level::Info);
        assert_eq!(
            row.detail,
            format!(
                "{} serves {}, not {}",
                env.label,
                env.another_home.display(),
                current.display()
            )
        );
        let row = daemon_check(Ok(status), &env.another_home, Some(&env.bind));
        assert_eq!(row.status, Level::Ok);
    }
}
