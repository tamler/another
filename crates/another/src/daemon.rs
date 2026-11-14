//! `another daemon`: keep `another serve` running in the background as a per-user launchd
//! LaunchAgent (macOS). The service binds loopback only, launchd restarts it whenever it
//! exits (including after `launchctl kickstart -k`), and its log goes to
//! `$ANOTHER_HOME/logs/serve.log`. The daemon sweeps expired holds and stale requests and
//! wakes idle Herdr agents that have new mail.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Serialize;

/// launchd label (reverse DNS of the project's home, toanother.one).
pub const DEFAULT_LABEL: &str = "one.toanother.another";

/// Where the service runs and what it runs.
#[derive(Debug, Clone)]
pub struct DaemonEnv {
    /// The user's home directory (LaunchAgents live under it).
    pub home: PathBuf,
    /// The Another data directory the service uses.
    pub another_home: PathBuf,
    /// The `another` binary the service runs.
    pub exe: PathBuf,
    /// `PATH` for the service, so it can find `herdr`, `tmux` and `cmux` the way your shell
    /// does. launchd's default PATH does not include ~/.local/bin or Homebrew.
    pub path: Option<String>,
    pub label: String,
    pub bind: String,
}

impl DaemonEnv {
    pub fn detect(another_home: PathBuf, label: String, bind: String) -> Result<DaemonEnv> {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .context("HOME is not set")?;
        Ok(DaemonEnv {
            home,
            another_home,
            exe: std::env::current_exe().context("locating the another binary")?,
            path: std::env::var("PATH").ok().filter(|p| !p.is_empty()),
            label,
            bind,
        })
    }

    pub fn plist_path(&self) -> PathBuf {
        self.home
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{}.plist", self.label))
    }

    pub fn log_path(&self) -> PathBuf {
        self.another_home.join("logs").join("serve.log")
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn validate_label(label: &str) -> Result<()> {
    let ok = !label.is_empty()
        && label.len() <= 128
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_');
    if !ok {
        bail!("invalid label {label:?}: use letters, digits, '.', '-' or '_'");
    }
    Ok(())
}

/// The LaunchAgent property list.
pub fn plist(env: &DaemonEnv) -> String {
    let s = |v: &str| format!("<string>{}</string>", xml_escape(v));
    let mut vars = format!(
        "    <key>ANOTHER_HOME</key>\n    {}\n",
        s(&env.another_home.to_string_lossy())
    );
    if let Some(path) = &env.path {
        vars.push_str(&format!("    <key>PATH</key>\n    {}\n", s(path)));
    }
    let log = env.log_path();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
  <key>Label</key>\n  {label}\n\
  <key>ProgramArguments</key>\n\
  <array>\n    {exe}\n    <string>serve</string>\n    <string>--bind</string>\n    {bind}\n  </array>\n\
  <key>EnvironmentVariables</key>\n\
  <dict>\n{vars}  </dict>\n\
  <key>RunAtLoad</key>\n  <true/>\n\
  <key>KeepAlive</key>\n  <true/>\n\
  <key>ThrottleInterval</key>\n  <integer>10</integer>\n\
  <key>StandardOutPath</key>\n  {log}\n\
  <key>StandardErrorPath</key>\n  {log}\n\
</dict>\n\
</plist>\n",
        label = s(&env.label),
        exe = s(&env.exe.to_string_lossy()),
        bind = s(&env.bind),
        log = s(&log.to_string_lossy()),
    )
}

/// The `--bind` value recorded in an installed plist.
fn bind_in(plist: &str) -> Option<String> {
    let after = plist.split("<string>--bind</string>").nth(1)?;
    let value = after.split("<string>").nth(1)?.split("</string>").next()?;
    Some(
        value
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    )
}

#[derive(Debug, Clone, Serialize)]
pub struct DaemonReport {
    pub label: String,
    pub plist: String,
    pub bind: String,
    pub log: String,
    /// `create`, `update`, `delete` or `unchanged` for the plist file.
    pub change: String,
    /// launchctl commands run (or that would run, with --dry-run).
    pub commands: Vec<Vec<String>>,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DaemonStatus {
    pub label: String,
    pub plist: String,
    pub installed: bool,
    pub loaded: bool,
    pub pid: Option<u32>,
    pub bind: Option<String>,
    /// `GET /health` answered on the bound address.
    pub healthy: bool,
    pub log: String,
}

fn ensure_macos() -> Result<()> {
    if cfg!(target_os = "macos") {
        Ok(())
    } else {
        bail!(
            "another daemon manages a launchd LaunchAgent, which is macOS only. Elsewhere, run \
             `another serve` under your service manager (docs/DAEMON.md has a systemd example)."
        )
    }
}

fn uid() -> Result<String> {
    let out = Command::new("/usr/bin/id")
        .arg("-u")
        .stdin(Stdio::null())
        .output()
        .context("running id -u")?;
    let uid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || uid.is_empty() || !uid.chars().all(|c| c.is_ascii_digit()) {
        bail!("could not read the user id");
    }
    Ok(uid)
}

fn launchctl(args: &[String]) -> Result<std::process::Output> {
    Command::new("/bin/launchctl")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("running launchctl")
}

fn domain_target(uid: &str, label: &str) -> String {
    format!("gui/{uid}/{label}")
}

/// Wait (up to 5 s) until launchd no longer knows `target`.
fn wait_unloaded(target: &str) {
    for _ in 0..50 {
        match launchctl(&["print".to_string(), target.to_string()]) {
            Ok(out) if out.status.success() => std::thread::sleep(Duration::from_millis(100)),
            _ => return,
        }
    }
}

fn write_private(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("another-tmp");
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&tmp, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // launchd refuses a plist that others can write.
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Write the LaunchAgent and (unless `load` is false) load it, replacing a loaded copy.
pub fn install(env: &DaemonEnv, load: bool, dry_run: bool) -> Result<DaemonReport> {
    ensure_macos()?;
    validate_label(&env.label)?;
    crate::serve::local_bind(&env.bind)
        .context("the daemon serves loopback only; pick a 127.0.0.1 or [::1] address")?;
    let path = env.plist_path();
    let text = plist(env);
    let change = match std::fs::read_to_string(&path) {
        Ok(current) if current == text => "unchanged",
        Ok(_) => "update",
        Err(_) => "create",
    };
    let mut commands = Vec::new();
    if load {
        let uid = uid()?;
        let target = domain_target(&uid, &env.label);
        commands.push(vec![
            "launchctl".to_string(),
            "bootout".into(),
            target.clone(),
        ]);
        commands.push(vec![
            "launchctl".to_string(),
            "bootstrap".into(),
            format!("gui/{uid}"),
            path.display().to_string(),
        ]);
    }
    if !dry_run {
        std::fs::create_dir_all(env.another_home.join("logs"))?;
        if change != "unchanged" {
            write_private(&path, &text)?;
        }
        for (i, argv) in commands.iter().enumerate() {
            let out = launchctl(&argv[1..])?;
            if i == 0 {
                // bootout fails when nothing is loaded yet, which is fine. When it did
                // unload something, wait until launchd has let go of the label, or the
                // bootstrap right after it can fail.
                if out.status.success() {
                    wait_unloaded(&argv[2]);
                }
                continue;
            }
            if !out.status.success() {
                bail!(
                    "launchctl {} failed: {}",
                    argv[1..].join(" "),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
        }
    }
    Ok(DaemonReport {
        label: env.label.clone(),
        plist: path.display().to_string(),
        bind: env.bind.clone(),
        log: env.log_path().display().to_string(),
        change: change.to_string(),
        commands,
        dry_run,
    })
}

/// Unload the LaunchAgent and delete its plist.
pub fn uninstall(env: &DaemonEnv, dry_run: bool) -> Result<DaemonReport> {
    ensure_macos()?;
    validate_label(&env.label)?;
    let path = env.plist_path();
    let uid = uid()?;
    let commands = vec![vec![
        "launchctl".to_string(),
        "bootout".into(),
        domain_target(&uid, &env.label),
    ]];
    let exists = path.exists();
    // Report the address the installed service used, not the default.
    let bind = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| bind_in(&t))
        .unwrap_or_else(|| env.bind.clone());
    if !dry_run {
        // bootout returns before launchd has let go; wait so `status` right after is true.
        if launchctl(&commands[0][1..])?.status.success() {
            wait_unloaded(&commands[0][2]);
        }
        if exists {
            std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
    }
    Ok(DaemonReport {
        label: env.label.clone(),
        plist: path.display().to_string(),
        bind,
        log: env.log_path().display().to_string(),
        change: if exists { "delete" } else { "unchanged" }.to_string(),
        commands,
        dry_run,
    })
}

/// Restart the loaded service (`launchctl kickstart -k`).
pub fn restart(env: &DaemonEnv) -> Result<()> {
    ensure_macos()?;
    validate_label(&env.label)?;
    let uid = uid()?;
    let out = launchctl(&[
        "kickstart".into(),
        "-k".into(),
        domain_target(&uid, &env.label),
    ])?;
    if !out.status.success() {
        bail!(
            "launchctl kickstart failed (is the daemon installed?): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn healthy(bind: &str) -> bool {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .proxy(None)
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(2)))
            .build(),
    );
    agent
        .get(&format!("http://{bind}/health"))
        .call()
        .is_ok_and(|r| r.status().is_success())
}

/// Is the service installed, loaded, running and answering?
pub fn status(env: &DaemonEnv) -> Result<DaemonStatus> {
    ensure_macos()?;
    validate_label(&env.label)?;
    let path = env.plist_path();
    let installed_text = std::fs::read_to_string(&path).ok();
    let uid = uid()?;
    let out = launchctl(&["print".into(), domain_target(&uid, &env.label)])?;
    let loaded = out.status.success();
    let text = String::from_utf8_lossy(&out.stdout);
    let pid = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("pid = "))
        .and_then(|p| p.trim().parse().ok());
    let bind = installed_text.as_deref().and_then(bind_in);
    let healthy = bind.as_deref().is_some_and(healthy);
    Ok(DaemonStatus {
        label: env.label.clone(),
        plist: path.display().to_string(),
        installed: installed_text.is_some(),
        loaded,
        pid,
        bind,
        healthy,
        log: env.log_path().display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> DaemonEnv {
        DaemonEnv {
            home: "/Users/someone".into(),
            another_home: "/Users/someone/.another".into(),
            exe: "/opt/another & co/another".into(),
            path: Some("/usr/bin:/bin:/opt/homebrew/bin".into()),
            label: DEFAULT_LABEL.into(),
            bind: "127.0.0.1:7466".into(),
        }
    }

    #[test]
    fn plist_runs_serve_on_loopback_with_keepalive() {
        let e = env();
        let text = plist(&e);
        assert!(text.starts_with("<?xml"));
        assert!(text.contains("<string>one.toanother.another</string>"));
        assert!(text.contains("<string>/opt/another &amp; co/another</string>"));
        assert!(text.contains("<string>serve</string>"));
        assert!(text.contains("<key>KeepAlive</key>\n  <true/>"));
        assert!(text.contains("<key>RunAtLoad</key>\n  <true/>"));
        let expected_log = Path::new("/Users/someone/.another")
            .join("logs")
            .join("serve.log");
        assert_eq!(
            text.matches(&format!("<string>{}</string>", expected_log.display()))
                .count(),
            2
        );
        assert!(text.contains("<key>PATH</key>"));
        assert_eq!(bind_in(&text).as_deref(), Some("127.0.0.1:7466"));
        assert_eq!(
            e.plist_path(),
            Path::new("/Users/someone")
                .join("Library")
                .join("LaunchAgents")
                .join("one.toanother.another.plist")
        );
    }

    #[test]
    fn labels_are_checked() {
        assert!(validate_label(DEFAULT_LABEL).is_ok());
        assert!(validate_label("one.toanother.another-test_2").is_ok());
        assert!(validate_label("../evil").is_err());
        assert!(validate_label("").is_err());
    }
}
