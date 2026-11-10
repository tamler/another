//! `another service`: an opt-in background service that runs `another serve` and restarts it
//! at login and reboot. Nothing here runs unless the user asks for it.
//!
//! - macOS: a launchd user agent (`~/Library/LaunchAgents/one.toanother.another.plist`,
//!   `RunAtLoad` + `KeepAlive`, logs in `$ANOTHER_HOME/logs/`), loaded with
//!   `launchctl bootstrap gui/$UID`.
//! - Linux: a systemd user unit (`~/.config/systemd/user/another.service`,
//!   `Restart=on-failure`), enabled with `systemctl --user enable --now`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;

use crate::serve::{check_bind, HttpOptions};

pub const LAUNCHD_LABEL: &str = "one.toanother.another";
pub const SYSTEMD_UNIT: &str = "another.service";
const LOG_OUT: &str = "another.out.log";
const LOG_ERR: &str = "another.err.log";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Launchd,
    Systemd,
}

impl Platform {
    /// The service manager for this OS. `ANOTHER_SERVICE_PLATFORM=launchd|systemd` overrides
    /// it (used by the tests to exercise both code paths on one machine).
    pub fn current() -> Result<Platform> {
        match std::env::var("ANOTHER_SERVICE_PLATFORM").ok().as_deref() {
            Some("launchd") => return Ok(Platform::Launchd),
            Some("systemd") => return Ok(Platform::Systemd),
            Some(other) if !other.is_empty() => {
                bail!("ANOTHER_SERVICE_PLATFORM must be launchd or systemd, not {other:?}")
            }
            _ => {}
        }
        if cfg!(target_os = "macos") {
            Ok(Platform::Launchd)
        } else if cfg!(target_os = "linux") {
            Ok(Platform::Systemd)
        } else {
            bail!("`another service` supports macOS (launchd) and Linux (systemd) only")
        }
    }
}

/// What the service runs: `another --home <home> serve <flags>`.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceSpec {
    pub exe: PathBuf,
    pub home: PathBuf,
    pub bind: String,
    pub auth: bool,
    pub allow_remote_bind: bool,
    pub allowed_hosts: Vec<String>,
    pub federation: bool,
}

impl ServiceSpec {
    pub fn args(&self) -> Vec<String> {
        let mut args = vec![
            self.exe.to_string_lossy().into_owned(),
            "--home".into(),
            self.home.to_string_lossy().into_owned(),
            "serve".into(),
            "--bind".into(),
            self.bind.clone(),
        ];
        if self.auth {
            args.push("--auth".into());
        }
        if self.allow_remote_bind {
            args.push("--allow-remote-bind".into());
        }
        for host in &self.allowed_hosts {
            args.push("--allowed-host".into());
            args.push(host.clone());
        }
        if self.federation {
            args.push("--federation".into());
        }
        args
    }

    pub fn log_dir(&self) -> PathBuf {
        self.home.join("logs")
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The launchd property list for the user agent.
pub fn plist(spec: &ServiceSpec) -> String {
    let string = |s: &str| format!("<string>{}</string>", xml_escape(s));
    let args: String = spec
        .args()
        .iter()
        .map(|a| format!("\n        {}", string(a)))
        .collect();
    let logs = spec.log_dir();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Written by `another service install`; remove with `another service uninstall`. -->
<plist version="1.0">
<dict>
    <key>Label</key>
    {label}
    <key>ProgramArguments</key>
    <array>{args}
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>ANOTHER_HOME</key>
        {home}
    </dict>
    <key>WorkingDirectory</key>
    {home}
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ProcessType</key>
    <string>Standard</string>
    <key>ThrottleInterval</key>
    <integer>30</integer>
    <key>StandardOutPath</key>
    {out}
    <key>StandardErrorPath</key>
    {err}
</dict>
</plist>
"#,
        label = string(LAUNCHD_LABEL),
        home = string(&spec.home.to_string_lossy()),
        out = string(&logs.join(LOG_OUT).to_string_lossy()),
        err = string(&logs.join(LOG_ERR).to_string_lossy()),
    )
}

/// Quote one word for a systemd `ExecStart=` line: double quotes, with backslashes and
/// quotes escaped, `%` doubled (specifiers) and `$` doubled (variable expansion).
fn systemd_quote(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
        .replace('$', "$$");
    format!("\"{escaped}\"")
}

/// Quote an `Environment=` assignment: like `ExecStart=`, but systemd does not expand `$`
/// there, so it is kept as is.
fn systemd_env(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

/// The systemd user unit.
pub fn unit(spec: &ServiceSpec) -> String {
    let exec = spec
        .args()
        .iter()
        .map(|a| systemd_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    let env = systemd_env(&format!("ANOTHER_HOME={}", spec.home.to_string_lossy()));
    format!(
        "# Written by `another service install`; remove with `another service uninstall`.\n\
         [Unit]\n\
         Description=Another (another serve): local inbox for coding agents\n\
         \n\
         [Service]\n\
         Type=simple\n\
         Environment={env}\n\
         WorkingDirectory={home}\n\
         ExecStart={exec}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        // WorkingDirectory= takes a bare absolute path (quotes are not stripped); only
        // specifiers need escaping.
        home = spec.home.to_string_lossy().replace('%', "%%"),
    )
}

/// Where the service definition lives.
pub fn definition_path(platform: Platform, user_home: &Path) -> PathBuf {
    match platform {
        Platform::Launchd => user_home
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{LAUNCHD_LABEL}.plist")),
        Platform::Systemd => {
            let config = std::env::var_os("XDG_CONFIG_HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| user_home.join(".config"));
            config.join("systemd").join("user").join(SYSTEMD_UNIT)
        }
    }
}

/// Read the serve arguments back out of an installed definition.
pub fn installed_args(platform: Platform, text: &str) -> Vec<String> {
    match platform {
        Platform::Launchd => {
            let Some(start) = text.find("<key>ProgramArguments</key>") else {
                return Vec::new();
            };
            let rest = &text[start..];
            let end = rest.find("</array>").unwrap_or(rest.len());
            rest[..end]
                .split("<string>")
                .skip(1)
                .filter_map(|s| s.split("</string>").next())
                .map(|s| {
                    s.replace("&lt;", "<")
                        .replace("&gt;", ">")
                        .replace("&quot;", "\"")
                        .replace("&apos;", "'")
                        .replace("&amp;", "&")
                })
                .collect()
        }
        Platform::Systemd => {
            let Some(line) = text.lines().find_map(|l| l.strip_prefix("ExecStart=")) else {
                return Vec::new();
            };
            parse_systemd_words(line)
        }
    }
}

fn parse_systemd_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut word = String::new();
        if c == '"' {
            chars.next();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        if let Some(n) = chars.next() {
                            word.push(n);
                        }
                    }
                    '"' => break,
                    _ => word.push(c),
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                word.push(c);
                chars.next();
            }
        }
        words.push(word.replace("%%", "%").replace("$$", "$"));
    }
    words
}

fn bind_of(args: &[String]) -> Option<String> {
    args.iter()
        .position(|a| a == "--bind")
        .and_then(|i| args.get(i + 1))
        .cloned()
}

// ---------------------------------------------------------------------------
// Running the service manager

struct Out {
    ok: bool,
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run(cmd: &str, args: &[&str]) -> Result<Out> {
    let out = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {cmd} (is it installed?)"))?;
    Ok(Out {
        ok: out.status.success(),
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

fn failure(cmd: &str, args: &[&str], out: &Out) -> anyhow::Error {
    anyhow!(
        "{cmd} {} failed: {}",
        args.join(" "),
        out.stderr.trim().lines().last().unwrap_or("no output")
    )
}

fn must(cmd: &str, args: &[&str]) -> Result<Out> {
    let out = run(cmd, args)?;
    if !out.ok {
        return Err(failure(cmd, args, &out));
    }
    Ok(out)
}

/// Replace a service definition in one step, so a failed write leaves the old one whole.
fn replace_file(path: &Path, text: &str) -> std::io::Result<()> {
    write_via(path, |f| f.write_all(text.as_bytes()))
}

fn write_via(
    path: &Path,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    let result = std::fs::File::create(&tmp)
        .and_then(|mut f| {
            write(&mut f)?;
            f.sync_all()
        })
        .and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn uid() -> Result<String> {
    let out = must("id", &["-u"])?;
    Ok(out.stdout.trim().to_string())
}

fn launchd_target() -> Result<String> {
    Ok(format!("gui/{}/{LAUNCHD_LABEL}", uid()?))
}

fn sleep_ms(ms: u64) {
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

/// Unload the launchd job and wait until launchd has really removed it. "Not loaded"
/// (3, 113) and "in progress" (36) count as success.
fn launchd_bootout() -> Result<()> {
    let target = launchd_target()?;
    let out = run("launchctl", &["bootout", &target])?;
    if !out.ok && !matches!(out.code, Some(3) | Some(36) | Some(113)) {
        return Err(failure("launchctl", &["bootout", &target], &out));
    }
    for _ in 0..50 {
        if !run("launchctl", &["print", &target])?.ok {
            return Ok(());
        }
        sleep_ms(100);
    }
    bail!("launchd is still unloading {LAUNCHD_LABEL}; try again in a few seconds")
}

/// Load the plist, retrying while launchd finishes a previous unload (5, 37).
fn launchd_bootstrap(plist: &Path) -> Result<()> {
    let domain = format!("gui/{}", uid()?);
    let path = plist.to_string_lossy().into_owned();
    let args = ["bootstrap", domain.as_str(), path.as_str()];
    let mut last = None;
    for attempt in 0..10 {
        let out = run("launchctl", &args)?;
        if out.ok {
            return Ok(());
        }
        if !matches!(out.code, Some(5) | Some(37)) {
            return Err(failure("launchctl", &args, &out));
        }
        last = Some(out);
        sleep_ms(200 * (attempt + 1));
    }
    Err(failure(
        "launchctl",
        &args,
        &last.expect("at least one attempt"),
    ))
}

/// (loaded, pid) as the service manager reports them.
fn manager_state(platform: Platform) -> Result<(bool, Option<u32>)> {
    match platform {
        Platform::Launchd => {
            let out = run("launchctl", &["print", &launchd_target()?])?;
            if !out.ok {
                return Ok((false, None));
            }
            let pid = out
                .stdout
                .lines()
                .find_map(|l| l.trim().strip_prefix("pid = "))
                .and_then(|p| p.trim().parse().ok());
            Ok((true, pid))
        }
        Platform::Systemd => {
            let active = run("systemctl", &["--user", "is-active", SYSTEMD_UNIT])?;
            let state = active.stdout.trim();
            let loaded = active.ok || state == "activating" || state == "reloading";
            let pid = run(
                "systemctl",
                &[
                    "--user",
                    "show",
                    "--property=MainPID",
                    "--value",
                    SYSTEMD_UNIT,
                ],
            )?
            .stdout
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|p| *p > 0);
            Ok((loaded, pid))
        }
    }
}

/// Make sure `systemctl --user` can reach a user manager before anything is written.
fn systemd_reachable() -> Result<()> {
    let out = run("systemctl", &["--user", "show-environment"])?;
    if out.ok {
        return Ok(());
    }
    bail!(
        "cannot reach the systemd user manager ({}). Use a login session with a user bus \
         (XDG_RUNTIME_DIR set), or run `loginctl enable-linger $USER` first",
        out.stderr.trim().lines().last().unwrap_or("no output")
    )
}

/// Is something already listening where `addr` would bind? Matches the same address or a
/// wildcard listener on the port (macOS lets a specific bind succeed alongside a wildcard
/// one, so a test bind alone is not enough). Returns (pid, command) of the listener.
fn port_taken(addr: std::net::SocketAddr) -> Result<Option<(Option<u32>, String)>> {
    let ip = addr.ip().to_string();
    let found = crate::scan::scan_listeners()
        .listeners
        .into_iter()
        .find(|l| {
            let host = l
                .address
                .rsplit_once(':')
                .map_or("", |(h, _)| h)
                .trim_matches(['[', ']']);
            l.port == addr.port()
                && (host == ip || matches!(host, "*" | "0.0.0.0" | "::" | "") || ip == "0.0.0.0")
        });
    if let Some(l) = found {
        let cmd = crate::scan::process_info(l.pid)
            .map(|p| p.command)
            .unwrap_or(l.command);
        return Ok(Some((Some(l.pid), cmd)));
    }
    // No scanner, or nothing seen: fall back to a test bind.
    match std::net::TcpListener::bind(addr) {
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            Ok(Some((None, "an unknown process".into())))
        }
        Err(e) => Err(anyhow!("cannot check {addr}: {e}")),
    }
}

#[derive(Debug, Serialize)]
pub struct InstallReport {
    pub platform: Platform,
    pub definition: String,
    /// `installed` or `updated`.
    pub action: String,
    pub args: Vec<String>,
    pub logs: String,
    /// What this update changed that was previously configured.
    pub warnings: Vec<String>,
    pub hints: Vec<String>,
}

/// Write (or update) the service definition and (re)start it.
pub fn install(spec: &ServiceSpec, user_home: &Path) -> Result<InstallReport> {
    let platform = Platform::current()?;
    let addr = check_bind(
        &spec.bind,
        &HttpOptions {
            auth: spec.auth,
            allow_remote_bind: spec.allow_remote_bind,
            ..HttpOptions::default()
        },
    )?;
    if platform == Platform::Systemd {
        systemd_reachable()?;
    }
    let path = definition_path(platform, user_home);
    let previous = std::fs::read_to_string(&path).ok();
    let (loaded, service_pid) = manager_state(platform)?;
    let previous_bind = previous
        .as_deref()
        .map(|t| installed_args(platform, t))
        .and_then(|a| bind_of(&a));

    // Refuse to fight another server for the port, unless it is this service being updated.
    if let Some((pid, cmd)) = port_taken(addr)? {
        let ours = loaded
            && match (pid, service_pid) {
                (Some(p), Some(s)) => p == s,
                // The scanner could not name the listener: trust the installed definition.
                (None, _) => previous_bind.as_deref() == Some(spec.bind.as_str()),
                _ => false,
            };
        if !ours {
            let what = if cmd.contains("another") && cmd.contains("serve") {
                format!("another `another serve` ({cmd}")
            } else {
                format!("another process ({cmd}")
            };
            let pid = pid.map(|p| format!(", pid {p}")).unwrap_or_default();
            bail!(
                "{what}{pid}) is already listening on {}; stop it or choose a different --bind",
                spec.bind
            );
        }
    }

    let text = match platform {
        Platform::Launchd => plist(spec),
        Platform::Systemd => unit(spec),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::create_dir_all(spec.log_dir())?;
    replace_file(&path, &text).with_context(|| format!("writing {}", path.display()))?;
    let path_s = path.to_string_lossy().into_owned();

    let activate = || -> Result<()> {
        match platform {
            Platform::Launchd => {
                if loaded {
                    // Reload so launchd picks up the new definition.
                    launchd_bootout()?;
                }
                launchd_bootstrap(&path)
            }
            Platform::Systemd => {
                must("systemctl", &["--user", "daemon-reload"])?;
                must("systemctl", &["--user", "enable", "--now", SYSTEMD_UNIT])?;
                if previous.is_some() {
                    must("systemctl", &["--user", "restart", SYSTEMD_UNIT])?;
                }
                Ok(())
            }
        }
    };
    if let Err(e) = activate() {
        return Err(e.context(roll_back(platform, &path, previous.as_deref(), loaded)));
    }

    let warnings = previous
        .as_deref()
        .map(|t| update_warnings(&installed_args(platform, t), spec))
        .unwrap_or_default();
    let mut hints = Vec::new();
    if platform == Platform::Systemd {
        hints.push(
            "On a headless box, run `loginctl enable-linger $USER` so the service starts at \
             boot without a login session."
                .into(),
        );
    }
    hints.push(
        "The service runs this binary by path; after moving or upgrading it, re-run \
         `another service install` with the same flags."
            .into(),
    );
    if !spec.auth {
        hints.push(
            "The service accepts unauthenticated MCP on loopback only. For remote access, install \
             with --auth (and --allowed-host for a tunnel hostname); see the README."
                .into(),
        );
    }
    Ok(InstallReport {
        platform,
        definition: path_s,
        action: if previous.is_some() {
            "updated"
        } else {
            "installed"
        }
        .into(),
        args: spec.args(),
        logs: match platform {
            Platform::Launchd => spec.log_dir().to_string_lossy().into_owned(),
            Platform::Systemd => format!("journalctl --user -u {SYSTEMD_UNIT}"),
        },
        warnings,
        hints,
    })
}

/// Undo a failed activation: remove a new definition, or restore the previous one and get
/// the service that was running back up. Returns what happened, for the error message.
fn roll_back(platform: Platform, path: &Path, previous: Option<&str>, was_loaded: bool) -> String {
    let systemd = platform == Platform::Systemd;
    let Some(old) = previous else {
        if systemd {
            // `enable --now` creates the enable link before it starts the unit.
            let _ = run("systemctl", &["--user", "disable", SYSTEMD_UNIT]);
        }
        let _ = std::fs::remove_file(path);
        if systemd {
            let _ = run("systemctl", &["--user", "daemon-reload"]);
        }
        return "the service could not be started; nothing was left installed".into();
    };
    if let Err(e) = replace_file(path, old) {
        return format!(
            "the update failed, and the previous definition could not be restored at {}: {e}",
            path.display()
        );
    }
    if systemd {
        let _ = run("systemctl", &["--user", "daemon-reload"]);
    }
    if !was_loaded {
        if systemd {
            // `enable --now` may have started the new definition before a later step failed.
            let _ = run("systemctl", &["--user", "stop", SYSTEMD_UNIT]);
        }
        return "the update failed; the previous definition was restored".into();
    }
    let restarted = match platform {
        // Still being loaded doesn't mean the old job survived: launchd may be partway through
        // the unload. Finish it, then load the restored definition.
        Platform::Launchd => launchd_bootout().and_then(|()| launchd_bootstrap(path)),
        Platform::Systemd => must("systemctl", &["--user", "restart", SYSTEMD_UNIT]).map(|_| ()),
    };
    match restarted {
        Ok(()) => "the update failed; the previous service was restored and is running".into(),
        Err(e) => format!(
            "the update failed; the previous service was restored but did not restart ({e}); \
             try `another service start`"
        ),
    }
}

/// What this update takes away from the installed service. Re-installing replaces every flag,
/// so an update run without the original flags would otherwise drop them silently.
fn update_warnings(old: &[String], new: &ServiceSpec) -> Vec<String> {
    let value = |flag: &str| {
        old.iter()
            .position(|a| a == flag)
            .and_then(|i| old.get(i + 1))
    };
    let has = |flag: &str| old.iter().any(|a| a == flag);
    let mut out = Vec::new();
    if has("--auth") && !new.auth {
        out.push(
            "the service no longer requires bearer tokens; the previous install used --auth"
                .to_string(),
        );
    }
    for host in old
        .windows(2)
        .filter(|w| w[0] == "--allowed-host")
        .map(|w| &w[1])
    {
        if !new.allowed_hosts.contains(host) {
            out.push(format!("--allowed-host {host} was dropped"));
        }
    }
    if has("--federation") && !new.federation {
        out.push("--federation was dropped; this node is no longer a federation hub".into());
    }
    if let Some(bind) = value("--bind").filter(|b| **b != new.bind) {
        out.push(format!(
            "the service moved from {bind} to {}; anything pointing at the old address, \
             such as a tunnel, needs updating",
            new.bind
        ));
    }
    let home = new.home.to_string_lossy();
    if let Some(old_home) = value("--home").filter(|h| **h != home) {
        out.push(format!(
            "the service now uses the data directory {home} instead of {old_home}"
        ));
    }
    out
}

#[derive(Debug, Serialize)]
pub struct UninstallReport {
    pub platform: Platform,
    pub definition: String,
    pub removed: bool,
    pub warnings: Vec<String>,
}

/// Stop the service and remove its definition. Safe to run when it is not installed.
/// A failed stop keeps the definition so the person can retry.
pub fn uninstall(user_home: &Path) -> Result<UninstallReport> {
    let platform = Platform::current()?;
    let path = definition_path(platform, user_home);
    let existed = path.exists();
    let warnings = Vec::new();
    match platform {
        Platform::Launchd => {
            let (loaded, _) = manager_state(platform)?;
            if loaded {
                launchd_bootout()?;
            }
        }
        Platform::Systemd => {
            if existed {
                systemd_reachable()
                    .and_then(|()| must("systemctl", &["--user", "disable", "--now", SYSTEMD_UNIT]))
                    .with_context(|| {
                        format!(
                            "the service may still be running; its definition was kept at {}. \
                             Retry `another service uninstall` from a login session with a user bus",
                            path.display()
                        )
                    })?;
            }
        }
    }
    if existed {
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    if platform == Platform::Systemd && existed {
        let _ = run("systemctl", &["--user", "daemon-reload"]);
        let _ = run("systemctl", &["--user", "reset-failed", SYSTEMD_UNIT]);
    }
    Ok(UninstallReport {
        platform,
        definition: path.to_string_lossy().into_owned(),
        removed: existed,
        warnings,
    })
}

#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub platform: Platform,
    pub installed: bool,
    pub definition: String,
    pub loaded: bool,
    pub pid: Option<u32>,
    pub bind: Option<String>,
    pub args: Vec<String>,
    pub log_lines: Vec<String>,
    /// Why the log lines couldn't be read, so an empty list isn't mistaken for quiet logs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_error: Option<String>,
}

fn tail(path: &Path, n: usize) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..]
        .iter()
        .map(|l| l.to_string())
        .collect()
}

/// Recent log lines from the service. On macOS `serve` logs to stderr, so that file is the
/// log; anything on stdout is shown after it, labelled.
pub fn log_lines(platform: Platform, home: &Path, n: usize) -> Result<Vec<String>> {
    Ok(match platform {
        Platform::Launchd => {
            let mut lines = tail(&home.join("logs").join(LOG_ERR), n);
            let out = tail(&home.join("logs").join(LOG_OUT), n);
            if !out.is_empty() {
                lines.push(format!("-- {LOG_OUT} --"));
                lines.extend(out);
            }
            lines
        }
        Platform::Systemd => {
            let out = run(
                "journalctl",
                &[
                    "--user",
                    "-u",
                    SYSTEMD_UNIT,
                    "-n",
                    &n.to_string(),
                    "--no-pager",
                    "-o",
                    "cat",
                ],
            )?;
            if !out.ok {
                anyhow::bail!("journalctl failed: {}", out.stderr.trim());
            }
            out.stdout.lines().map(str::to_string).collect()
        }
    })
}

pub fn status(user_home: &Path, home: &Path, lines: usize) -> Result<StatusReport> {
    let platform = Platform::current()?;
    let path = definition_path(platform, user_home);
    let text = std::fs::read_to_string(&path).ok();
    let args = text
        .as_deref()
        .map(|t| installed_args(platform, t))
        .unwrap_or_default();
    // Logs live under the home the service was installed with.
    let service_home = args
        .iter()
        .position(|a| a == "--home")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .unwrap_or_else(|| home.to_path_buf());
    let (loaded, pid) = if text.is_some() {
        manager_state(platform)?
    } else {
        (false, None)
    };
    let logs = if text.is_some() {
        log_lines(platform, &service_home, lines)
    } else {
        Ok(Vec::new())
    };
    Ok(StatusReport {
        platform,
        installed: text.is_some(),
        definition: path.to_string_lossy().into_owned(),
        loaded,
        pid,
        bind: bind_of(&args),
        log_lines: logs.as_ref().cloned().unwrap_or_default(),
        log_error: logs.err().map(|e| format!("{e:#}")),
        args,
    })
}

/// Start or stop an installed service (it stays installed either way).
pub fn start_stop(user_home: &Path, start: bool) -> Result<()> {
    let platform = Platform::current()?;
    let path = definition_path(platform, user_home);
    if !path.exists() {
        bail!("the service is not installed; run `another service install` first");
    }
    match (platform, start) {
        (Platform::Launchd, true) => {
            let (loaded, _) = manager_state(platform)?;
            if loaded {
                must("launchctl", &["kickstart", &launchd_target()?])?;
            } else {
                launchd_bootstrap(&path)?;
            }
        }
        (Platform::Launchd, false) => {
            // KeepAlive would restart a killed process, so unload it; it loads again at the
            // next login (RunAtLoad) or with `another service start`.
            launchd_bootout()?;
        }
        (Platform::Systemd, true) => {
            must("systemctl", &["--user", "start", SYSTEMD_UNIT])?;
        }
        (Platform::Systemd, false) => {
            must("systemctl", &["--user", "stop", SYSTEMD_UNIT])?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_write_keeps_the_previous_definition() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("another.service");
        std::fs::write(&path, "previous").unwrap();

        let failed = write_via(&path, |f| {
            f.write_all(b"partial")?;
            Err(std::io::Error::other("disk full"))
        });
        assert!(failed.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "previous");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

        replace_file(&path, "next").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "next");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    fn spec() -> ServiceSpec {
        ServiceSpec {
            exe: PathBuf::from("/opt/My Tools/another"),
            home: PathBuf::from("/work/Another Data/home"),
            bind: "127.0.0.1:7499".into(),
            auth: true,
            allow_remote_bind: false,
            allowed_hosts: vec!["box.tail-net.ts.net".into(), "a&b<c>\"q\"'s".into()],
            federation: false,
        }
    }

    #[test]
    fn args_carry_serve_flags() {
        let a = spec().args();
        assert_eq!(a[0], "/opt/My Tools/another");
        assert_eq!(&a[1..4], ["--home", "/work/Another Data/home", "serve"]);
        assert!(a.windows(2).any(|w| w == ["--bind", "127.0.0.1:7499"]));
        assert!(a.contains(&"--auth".to_string()));
        assert!(a
            .windows(2)
            .any(|w| w == ["--allowed-host", "box.tail-net.ts.net"]));
        assert!(!a.contains(&"--federation".to_string()));
    }

    #[test]
    fn plist_escapes_and_round_trips() {
        let s = spec();
        let p = plist(&s);
        assert!(p.contains("<string>one.toanother.another</string>"));
        assert!(p.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(p.contains("<key>KeepAlive</key>\n    <true/>"));
        assert!(p.contains("<string>/opt/My Tools/another</string>"));
        assert!(p.contains("<string>a&amp;b&lt;c&gt;&quot;q&quot;&apos;s</string>"));
        assert!(!p.contains("a&b<c>"), "raw special characters never appear");
        assert!(p.contains("<string>/work/Another Data/home/logs/another.err.log</string>"));
        assert!(p.contains("<key>ANOTHER_HOME</key>"));
        assert_eq!(installed_args(Platform::Launchd, &p), s.args());
    }

    #[test]
    fn unit_quotes_and_round_trips() {
        let mut s = spec();
        s.allowed_hosts.push(r"100%$HOME\x".into());
        let u = unit(&s);
        assert!(u.contains("Restart=on-failure"));
        assert!(u.contains("WantedBy=default.target"));
        assert!(u.contains("\"/opt/My Tools/another\""));
        assert!(u.contains(r#""a&b<c>\"q\"'s""#));
        assert!(u.contains(r#""100%%$$HOME\\x""#), "{u}");
        assert!(u.contains("Environment=\"ANOTHER_HOME=/work/Another Data/home\""));
        assert!(
            u.contains("WorkingDirectory=/work/Another Data/home\n"),
            "{u}"
        );
        assert!(!u.contains("After="));
        let mut dollar = spec();
        dollar.home = PathBuf::from("/work/a$b%c");
        let d = unit(&dollar);
        assert!(
            d.contains("Environment=\"ANOTHER_HOME=/work/a$b%%c\""),
            "{d}"
        );
        assert!(d.contains("WorkingDirectory=/work/a$b%%c\n"), "{d}");
        assert!(d.contains("\"/work/a$$b%%c\""), "ExecStart doubles $: {d}");
        assert_eq!(installed_args(Platform::Systemd, &u), s.args());
        assert_eq!(bind_of(&s.args()).as_deref(), Some("127.0.0.1:7499"));
    }

    #[test]
    fn definition_paths() {
        let home = Path::new("/work/u");
        assert_eq!(
            definition_path(Platform::Launchd, home),
            PathBuf::from("/work/u/Library/LaunchAgents/one.toanother.another.plist")
        );
        if std::env::var_os("XDG_CONFIG_HOME").is_none() {
            assert_eq!(
                definition_path(Platform::Systemd, home),
                PathBuf::from("/work/u/.config/systemd/user/another.service")
            );
        }
    }
}
