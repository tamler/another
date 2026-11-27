//! `another daemon`: keep `another serve` running in the background for the current user.
//!
//! - macOS: a launchd LaunchAgent in `gui/<uid>` (`RunAtLoad`, `KeepAlive`).
//! - Linux: a systemd **user** unit, enabled for `default.target` (`Restart=always`).
//! - Windows: a Task Scheduler task that starts at the user's logon.
//!
//! The service binds loopback only, and its log goes to `$ANOTHER_HOME/logs/serve.log`. The
//! daemon sweeps expired holds and stale requests and wakes idle Herdr agents that have new
//! mail. The same `install`, `uninstall`, `status` and `restart` verbs work everywhere.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Serialize;

/// Service label: the launchd label, the systemd unit name (plus `.service`) and the
/// Task Scheduler task name (reverse DNS of the project's home, toanother.one).
pub const DEFAULT_LABEL: &str = "one.toanother.another";

/// The service manager that runs the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Manager {
    Launchd,
    Systemd,
    TaskScheduler,
}

impl Manager {
    /// The manager for the platform this binary was built for.
    pub fn current() -> Result<Manager> {
        if cfg!(target_os = "macos") {
            Ok(Manager::Launchd)
        } else if cfg!(target_os = "linux") {
            Ok(Manager::Systemd)
        } else if cfg!(windows) {
            Ok(Manager::TaskScheduler)
        } else {
            bail!(
                "another daemon supports macOS, Linux and Windows. Elsewhere, run \
                 `another serve` under your service manager (docs/DAEMON.md)."
            )
        }
    }
}

/// Where the service runs and what it runs.
#[derive(Debug, Clone)]
pub struct DaemonEnv {
    pub manager: Manager,
    /// The user's home directory (LaunchAgents and the default systemd folder live under it).
    pub home: PathBuf,
    /// `$XDG_CONFIG_HOME`, when set to an absolute path (systemd user units).
    pub config_home: Option<PathBuf>,
    /// The Another data directory the service uses.
    pub another_home: PathBuf,
    /// The `another` binary the service runs.
    pub exe: PathBuf,
    /// `PATH` for the service, so it can find `herdr`, `tmux` and `cmux` the way your shell
    /// does. Neither launchd's nor systemd's default PATH includes ~/.local/bin or Homebrew.
    /// Not used on Windows, where a task inherits the user's environment.
    pub path: Option<String>,
    /// `DOMAIN\user` for the Windows logon trigger and task principal.
    pub user: Option<String>,
    pub label: String,
    pub bind: String,
}

fn non_empty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

impl DaemonEnv {
    pub fn detect(another_home: PathBuf, label: String, bind: String) -> Result<DaemonEnv> {
        let manager = Manager::current()?;
        let home = non_empty("HOME")
            .or_else(|| non_empty("USERPROFILE"))
            .map(PathBuf::from)
            .context("HOME is not set")?;
        let user = non_empty("USERNAME").map(|name| match non_empty("USERDOMAIN") {
            Some(domain) => format!("{domain}\\{name}"),
            None => name,
        });
        Ok(DaemonEnv {
            manager,
            home,
            config_home: non_empty("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute()),
            another_home,
            exe: std::env::current_exe().context("locating the another binary")?,
            path: non_empty("PATH"),
            user,
            label,
            bind,
        })
    }

    fn unit_name(&self) -> String {
        format!("{}.service", self.label)
    }

    /// The definition file: plist, systemd unit or task XML.
    pub fn file_path(&self) -> PathBuf {
        match self.manager {
            Manager::Launchd => self
                .home
                .join("Library")
                .join("LaunchAgents")
                .join(format!("{}.plist", self.label)),
            Manager::Systemd => self
                .config_home
                .clone()
                .unwrap_or_else(|| self.home.join(".config"))
                .join("systemd")
                .join("user")
                .join(self.unit_name()),
            Manager::TaskScheduler => self
                .another_home
                .join("daemon")
                .join(format!("{}.xml", self.label)),
        }
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

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
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

/// Refuse a value that no definition format can carry on one line.
fn plain(what: &str, value: &str) -> Result<()> {
    if value.chars().any(char::is_control) {
        bail!("{what} {value:?} contains a control character; the daemon cannot record it");
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

/// One systemd word in double quotes: backslash and quote escaped, and `%` specifiers
/// doubled so the value is taken literally. `$` is left alone: `Environment=` never expands
/// it, and in `ExecStart=` systemd resolves the executable path (the only quoted word here)
/// before it expands variables in the arguments, so `$$` would name a different file.
fn systemd_quote(s: &str) -> String {
    let out = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{out}\"")
}

/// The systemd user unit.
pub fn systemd_unit(env: &DaemonEnv) -> Result<String> {
    let exe = env.exe.to_string_lossy();
    let home = env.another_home.to_string_lossy();
    let log = env.log_path();
    let log = log.to_string_lossy();
    plain("the another binary path", &exe)?;
    plain("ANOTHER_HOME", &home)?;
    plain("the log path", &log)?;
    let mut environment = format!(
        "Environment={}\n",
        systemd_quote(&format!("ANOTHER_HOME={home}"))
    );
    if let Some(path) = &env.path {
        plain("PATH", path)?;
        environment.push_str(&format!(
            "Environment={}\n",
            systemd_quote(&format!("PATH={path}"))
        ));
    }
    let log = log.replace('%', "%%");
    Ok(format!(
        "# Written by `another daemon install`; install rewrites it and uninstall removes it.\n\
[Unit]\n\
Description=Another: another serve on loopback for this user\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart={exe} serve --bind {bind}\n\
{environment}\
Restart=always\n\
RestartSec=10\n\
StandardOutput=append:{log}\n\
StandardError=append:{log}\n\
\n\
[Install]\n\
WantedBy=default.target\n",
        exe = systemd_quote(&exe),
        bind = env.bind,
    ))
}

/// One argument for a Windows command line: in double quotes, with the backslashes in front
/// of the closing quote doubled so they stay backslashes. Paths cannot contain `"`.
fn windows_quote(s: &str) -> String {
    let trailing = s.len() - s.trim_end_matches('\\').len();
    format!("\"{s}{}\"", "\\".repeat(trailing))
}

/// The Task Scheduler task: start `another serve` at this user's logon, as this user, with
/// no time limit, and restart it if it fails.
pub fn task_xml(env: &DaemonEnv) -> Result<String> {
    let user = env
        .user
        .as_deref()
        .context("USERNAME is not set; the logon task needs the current user")?;
    let exe = env.exe.to_string_lossy();
    let home = env.another_home.to_string_lossy();
    let log = env.log_path();
    let log = log.to_string_lossy();
    for (what, value) in [
        ("the another binary path", &*exe),
        ("ANOTHER_HOME", &*home),
        ("the log path", &*log),
        ("the user name", user),
    ] {
        plain(what, value)?;
        if value.contains('"') {
            bail!("{what} {value:?} contains a double quote; the daemon cannot record it");
        }
    }
    let arguments = format!(
        "--home {} serve --bind {} --log-file {}",
        windows_quote(&home),
        env.bind,
        windows_quote(&log)
    );
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\r\n\
<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\r\n\
  <RegistrationInfo>\r\n\
    <Description>Another: another serve on loopback for this user. Written by another daemon install.</Description>\r\n\
  </RegistrationInfo>\r\n\
  <Triggers>\r\n\
    <LogonTrigger>\r\n\
      <Enabled>true</Enabled>\r\n\
      <UserId>{user}</UserId>\r\n\
    </LogonTrigger>\r\n\
  </Triggers>\r\n\
  <Principals>\r\n\
    <Principal id=\"Author\">\r\n\
      <UserId>{user}</UserId>\r\n\
      <LogonType>InteractiveToken</LogonType>\r\n\
      <RunLevel>LeastPrivilege</RunLevel>\r\n\
    </Principal>\r\n\
  </Principals>\r\n\
  <Settings>\r\n\
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\r\n\
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\r\n\
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\r\n\
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>\r\n\
    <RestartOnFailure>\r\n\
      <Interval>PT1M</Interval>\r\n\
      <Count>999</Count>\r\n\
    </RestartOnFailure>\r\n\
    <Enabled>true</Enabled>\r\n\
  </Settings>\r\n\
  <Actions Context=\"Author\">\r\n\
    <Exec>\r\n\
      <Command>{exe}</Command>\r\n\
      <Arguments>{arguments}</Arguments>\r\n\
    </Exec>\r\n\
  </Actions>\r\n\
</Task>\r\n",
        user = xml_escape(user),
        exe = xml_escape(&exe),
        arguments = xml_escape(&arguments),
    ))
}

/// UTF-16 LE with a byte order mark, the encoding `schtasks /XML` reads reliably.
fn utf16(text: &str) -> Vec<u8> {
    let mut out = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

fn from_utf16(bytes: &[u8]) -> Option<String> {
    let body = bytes.strip_prefix(&[0xFF, 0xFE])?;
    let (pairs, _) = body.as_chunks::<2>();
    let units: Vec<u16> = pairs.iter().map(|c| u16::from_le_bytes(*c)).collect();
    String::from_utf16(&units).ok()
}

/// The definition file's bytes for this manager.
fn definition(env: &DaemonEnv) -> Result<Vec<u8>> {
    Ok(match env.manager {
        Manager::Launchd => plist(env).into_bytes(),
        Manager::Systemd => systemd_unit(env)?.into_bytes(),
        Manager::TaskScheduler => utf16(&task_xml(env)?),
    })
}

/// An installed definition, as text.
fn read_definition(manager: Manager, path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    match manager {
        Manager::TaskScheduler => from_utf16(&bytes),
        _ => String::from_utf8(bytes).ok(),
    }
}

/// The `--bind` value recorded in an installed definition.
fn bind_in(manager: Manager, text: &str) -> Option<String> {
    match manager {
        Manager::Launchd => {
            let after = text.split("<string>--bind</string>").nth(1)?;
            let value = after.split("<string>").nth(1)?.split("</string>").next()?;
            Some(xml_unescape(value))
        }
        Manager::Systemd => {
            let exec = text.lines().find(|l| l.starts_with("ExecStart="))?;
            let after = exec.split(" --bind ").nth(1)?;
            after.split_whitespace().next().map(str::to_string)
        }
        Manager::TaskScheduler => {
            let args = text
                .split("<Arguments>")
                .nth(1)?
                .split("</Arguments>")
                .next()?;
            let args = xml_unescape(args);
            let after = args.split(" --bind ").nth(1)?;
            after.split_whitespace().next().map(str::to_string)
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DaemonReport {
    pub manager: Manager,
    pub label: String,
    /// The definition file (plist, systemd unit or task XML).
    pub file: String,
    /// The same path on macOS, under the key earlier releases used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plist: Option<String>,
    pub bind: String,
    pub log: String,
    /// `create`, `update`, `delete` or `unchanged` for the definition file.
    pub change: String,
    /// Service-manager commands run (or that would run, with --dry-run).
    pub commands: Vec<Vec<String>>,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DaemonStatus {
    pub manager: Manager,
    pub label: String,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plist: Option<String>,
    pub installed: bool,
    /// Loaded by launchd for this home, loaded and enabled in the systemd user manager,
    /// or registered with Task Scheduler.
    pub loaded: bool,
    /// Why the service or its health response could not be attributed to this home.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The running process, where the manager reports one (launchd and systemd).
    pub pid: Option<u32>,
    pub bind: Option<String>,
    /// `GET /health` answered on the bound address and identified this home's node.
    pub healthy: bool,
    pub log: String,
}

fn macos_plist_key(env: &DaemonEnv) -> Option<String> {
    (env.manager == Manager::Launchd).then(|| env.file_path().display().to_string())
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

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// Run one recorded command. launchd's tool is called by its fixed path; `systemctl` and
/// `schtasks` are found on PATH, where the system puts them.
fn exec(argv: &[String]) -> Result<Output> {
    let program = match argv[0].as_str() {
        "launchctl" => "/bin/launchctl",
        other => other,
    };
    Command::new(program)
        .args(&argv[1..])
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running {}", argv[0]))
}

/// Run a command that has to succeed. A systemd error that means "no user manager here"
/// gets an explanation of what that is and how to get one.
fn must(argv: &[String]) -> Result<Output> {
    let out = exec(argv)?;
    if out.status.success() {
        return Ok(out);
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let mut message = format!("{} failed: {stderr}", argv.join(" "));
    if argv[0] == "systemctl"
        && ["bus", "XDG_RUNTIME_DIR"]
            .iter()
            .any(|w| stderr.contains(w))
    {
        message.push_str(
            ". another daemon runs under your systemd user manager (systemctl --user), which \
             this session cannot reach. Run it from a login session, start the manager at \
             boot with `loginctl enable-linger $USER`, or run `another serve` under your own \
             service manager.",
        );
    }
    bail!(message)
}

fn launchd_target(uid: &str, label: &str) -> String {
    format!("gui/{uid}/{label}")
}

/// Wait (up to 5 s) until launchd no longer knows `target`.
fn wait_unloaded(target: &str) {
    for _ in 0..50 {
        match exec(&argv(&["launchctl", "print", target])) {
            Ok(out) if out.status.success() => std::thread::sleep(Duration::from_millis(100)),
            _ => return,
        }
    }
}

/// Atomically write a daemon definition file. Temp names are unique per write, synced before
/// rename, and stale sibling temp files are swept after rename; concurrent installs are not
/// supported.
fn write_definition(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::atomic_write::write_file(path, bytes, None, |tmp| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // launchd refuses a plist that others can write; systemd warns about one.
            std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o644))?;
        }
        #[cfg(not(unix))]
        let _ = tmp;
        Ok(())
    })
}

/// The commands that load (or reload) an installed definition.
fn load_commands(env: &DaemonEnv) -> Result<Vec<Vec<String>>> {
    Ok(match env.manager {
        Manager::Launchd => {
            let uid = uid()?;
            vec![
                argv(&["launchctl", "bootout", &launchd_target(&uid, &env.label)]),
                argv(&[
                    "launchctl",
                    "bootstrap",
                    &format!("gui/{uid}"),
                    &env.file_path().display().to_string(),
                ]),
            ]
        }
        Manager::Systemd => {
            let unit = env.unit_name();
            vec![
                argv(&["systemctl", "--user", "daemon-reload"]),
                argv(&["systemctl", "--user", "enable", &unit]),
                argv(&["systemctl", "--user", "restart", &unit]),
            ]
        }
        Manager::TaskScheduler => vec![
            argv(&[
                "schtasks",
                "/Create",
                "/TN",
                &env.label,
                "/XML",
                &env.file_path().display().to_string(),
                "/F",
            ]),
            argv(&["schtasks", "/Run", "/TN", &env.label]),
        ],
    })
}

fn report(
    env: &DaemonEnv,
    bind: String,
    change: &str,
    commands: Vec<Vec<String>>,
    dry_run: bool,
) -> DaemonReport {
    DaemonReport {
        manager: env.manager,
        label: env.label.clone(),
        file: env.file_path().display().to_string(),
        plist: macos_plist_key(env),
        bind,
        log: env.log_path().display().to_string(),
        change: change.to_string(),
        commands,
        dry_run,
    }
}

/// Write the definition and (unless `load` is false) load it, replacing a loaded copy.
pub fn install(env: &DaemonEnv, load: bool, dry_run: bool) -> Result<DaemonReport> {
    validate_label(&env.label)?;
    crate::serve::local_bind(&env.bind)
        .context("the daemon serves loopback only; pick a 127.0.0.1 or [::1] address")?;
    let path = env.file_path();
    let bytes = definition(env)?;
    let change = match std::fs::read(&path) {
        Ok(current) if current == bytes => "unchanged",
        Ok(_) => "update",
        Err(_) => "create",
    };
    let commands = if load {
        load_commands(env)?
    } else {
        Vec::new()
    };
    eprintln!("{}", crate::serve::LOCAL_LISTENER_WARNING);
    if !dry_run {
        std::fs::create_dir_all(env.another_home.join("logs"))?;
        if change != "unchanged" {
            write_definition(&path, &bytes)?;
        }
        for (i, cmd) in commands.iter().enumerate() {
            if env.manager == Manager::Launchd && i == 0 {
                // bootout fails when nothing is loaded yet, which is fine. When it did
                // unload something, wait until launchd has let go of the label, or the
                // bootstrap right after it can fail.
                if exec(cmd)?.status.success() {
                    wait_unloaded(&cmd[2]);
                }
                continue;
            }
            must(cmd)?;
        }
    }
    Ok(report(env, env.bind.clone(), change, commands, dry_run))
}

/// Unload the service and delete its definition.
pub fn uninstall(env: &DaemonEnv, dry_run: bool) -> Result<DaemonReport> {
    validate_label(&env.label)?;
    let path = env.file_path();
    let exists = path.exists();
    // Report the address the installed service used, not the default.
    let bind = read_definition(env.manager, &path)
        .and_then(|t| bind_in(env.manager, &t))
        .unwrap_or_else(|| env.bind.clone());
    let unit = env.unit_name();
    let commands = match env.manager {
        Manager::Launchd => vec![argv(&[
            "launchctl",
            "bootout",
            &launchd_target(&uid()?, &env.label),
        ])],
        Manager::Systemd => vec![
            argv(&["systemctl", "--user", "disable", "--now", &unit]),
            argv(&["systemctl", "--user", "daemon-reload"]),
        ],
        Manager::TaskScheduler => vec![
            argv(&["schtasks", "/End", "/TN", &env.label]),
            argv(&["schtasks", "/Delete", "/TN", &env.label, "/F"]),
        ],
    };
    if !dry_run {
        match env.manager {
            Manager::Launchd => {
                // bootout returns before launchd has let go; wait so `status` right after
                // is true.
                if exec(&commands[0])?.status.success() {
                    wait_unloaded(&commands[0][2]);
                }
                remove(&path, exists)?;
            }
            Manager::Systemd => {
                // Disabling fails for a unit the manager never loaded. That is fine only
                // when the manager then confirms the unit is stopped and not enabled.
                let disabled = exec(&commands[0])?;
                if !disabled.status.success() {
                    confirm_gone(env, unit_presence(&unit), &disabled)?;
                }
                remove(&path, exists)?;
                let reloaded = exec(&commands[1])?;
                if !reloaded.status.success() {
                    bail!(
                        "the unit is stopped and {} was removed, but `systemctl --user \
                         daemon-reload` failed: {}. Run it again so the manager forgets the unit.",
                        path.display(),
                        stderr_of(&reloaded)
                    );
                }
            }
            Manager::TaskScheduler => {
                // Deleting a task does not stop a program it is running, so the task has
                // to be shown stopped before it is deleted. A task Task Scheduler confirms
                // is not registered has nothing to stop or delete.
                match task_run_state(&env.label) {
                    TaskRun::Absent => {}
                    TaskRun::Unknown(why) => {
                        bail!(
                            "could not read the state of the {} task: {why}. Nothing was stopped \
                             or deleted, and {} was kept; run `another daemon uninstall` again \
                             once Task Scheduler answers.",
                            env.label,
                            path.display()
                        )
                    }
                    TaskRun::Running | TaskRun::Stopped => {
                        // `/End` fails for a task that is not running; the state check
                        // after it decides, not its exit status.
                        let ended = exec(&commands[0])?;
                        let (state, taken) = wait_task_down(&env.label, &bind);
                        match state {
                            TaskRun::Stopped | TaskRun::Absent if taken => bail!(
                                "the {} task stopped, but {bind} is still in use, so the server \
                                 it started may still be running, even if it does not answer \
                                 (or another program uses that address). Nothing was deleted, \
                                 and {} was kept; stop that server and run `another daemon \
                                 uninstall` again.",
                                env.label,
                                path.display()
                            ),
                            TaskRun::Stopped | TaskRun::Absent => {}
                            TaskRun::Running => bail!(
                                "the {} task is still running: `schtasks /End` said {}. It was \
                                 not deleted, because deleting a task leaves its program \
                                 running, and {} was kept; stop it (or fix the error) and run \
                                 `another daemon uninstall` again.",
                                env.label,
                                stderr_of(&ended),
                                path.display()
                            ),
                            TaskRun::Unknown(why) => bail!(
                                "could not confirm that the {} task stopped (`schtasks /End` \
                                 said {}; reading its state failed: {why}). It was not deleted, \
                                 and {} was kept; run `another daemon uninstall` again once Task \
                                 Scheduler answers.",
                                env.label,
                                stderr_of(&ended),
                                path.display()
                            ),
                        }
                        // Deleting fails for a task that is gone by now; that is fine only
                        // when the task list confirms it.
                        let deleted = exec(&commands[1])?;
                        if !deleted.status.success() {
                            confirm_gone(env, task_presence(&env.label), &deleted)?;
                        }
                    }
                }
                remove(&path, exists)?;
            }
        }
    }
    Ok(report(
        env,
        bind,
        if exists { "delete" } else { "unchanged" },
        commands,
        dry_run,
    ))
}

fn remove(path: &Path, exists: bool) -> Result<()> {
    if exists {
        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
    }
    crate::atomic_write::sweep_stale_temps(path);
    Ok(())
}

/// What the service manager says about the service. `Unknown` (it could not be asked, or
/// refused) is never taken as absence.
#[derive(Debug, PartialEq, Eq)]
enum Presence {
    /// Running, enabled or registered.
    Present,
    /// The manager answered, and the service is stopped and not enabled, or not there.
    Absent,
    Unknown(String),
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_string()
}

/// After a stop or delete command failed: go on only when the manager confirms the service
/// is gone. Otherwise keep the definition, which is what a later uninstall or a manual
/// cleanup needs, and say so.
fn confirm_gone(env: &DaemonEnv, presence: Presence, failed: &Output) -> Result<()> {
    let file = env.file_path();
    match presence {
        Presence::Absent => Ok(()),
        Presence::Present => bail!(
            "could not stop and remove the {} service: {}. It is still running, enabled or \
             registered, so {} was kept; fix the error and run `another daemon uninstall` again.",
            env.label,
            stderr_of(failed),
            file.display()
        ),
        Presence::Unknown(why) => bail!(
            "could not confirm that the {} service is stopped and removed: {} (and asking the \
             service manager failed: {why}). {} was kept, so `another daemon uninstall` can run \
             again once the manager answers; delete it by hand only if you know the service is gone.",
            env.label,
            stderr_of(failed),
            file.display()
        ),
    }
}

/// A systemd user unit: running or enabled is present; stopped and not enabled (including
/// a unit the manager does not know) is absent.
fn unit_presence(unit: &str) -> Presence {
    match exec(&argv(&[
        "systemctl",
        "--user",
        "show",
        unit,
        "--property=LoadState,ActiveState,UnitFileState",
    ])) {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            let value = |key: &str| {
                text.lines()
                    .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
                    .map(str::trim)
                    .unwrap_or_default()
                    .to_string()
            };
            let running = matches!(
                value("ActiveState").as_str(),
                "active" | "activating" | "reloading" | "deactivating"
            );
            let enabled = matches!(
                value("UnitFileState").as_str(),
                "enabled" | "enabled-runtime" | "linked" | "linked-runtime"
            );
            if running || enabled {
                Presence::Present
            } else {
                Presence::Absent
            }
        }
        Ok(out) => Presence::Unknown(stderr_of(&out)),
        Err(e) => Presence::Unknown(format!("{e:#}")),
    }
}

/// Task Scheduler's code for "the task is currently running" (`SCHED_S_TASK_RUNNING`,
/// 0x41301). `schtasks /Query /V` shows it as the task's last result while it runs. It is a
/// number, so unlike the status column it reads the same in every system language.
const SCHED_S_TASK_RUNNING: &str = "267009";

/// Whether a Task Scheduler task is running, from `schtasks /Query /TN <label> /V /FO CSV
/// /NH` (its last-result column). `Absent` only when the task list confirms the task is
/// not registered; a query that fails for any other reason is `Unknown`.
#[derive(Debug, PartialEq, Eq)]
enum TaskRun {
    Running,
    Stopped,
    Absent,
    Unknown(String),
}

fn task_run_state(label: &str) -> TaskRun {
    match exec(&argv(&[
        "schtasks", "/Query", "/TN", label, "/V", "/FO", "CSV", "/NH",
    ])) {
        Ok(out) if out.status.success() => {
            let want = format!("\\{label}");
            let text = String::from_utf8_lossy(&out.stdout);
            // One row per trigger; the task name is the second column and the last
            // result the seventh.
            let rows: Vec<Vec<String>> = text
                .lines()
                .map(csv_fields)
                .filter(|f| f.get(1).map(String::as_str) == Some(want.as_str()))
                .collect();
            if rows.is_empty() {
                return TaskRun::Unknown(format!("schtasks did not list {want}"));
            }
            if rows
                .iter()
                .any(|f| f.get(6).map(|r| r.trim()) == Some(SCHED_S_TASK_RUNNING))
            {
                TaskRun::Running
            } else {
                TaskRun::Stopped
            }
        }
        Ok(out) => match task_presence(label) {
            Presence::Absent => TaskRun::Absent,
            Presence::Present => TaskRun::Unknown(stderr_of(&out)),
            Presence::Unknown(why) => TaskRun::Unknown(why),
        },
        Err(e) => TaskRun::Unknown(format!("{e:#}")),
    }
}

/// After `/End`: wait up to ten seconds until Task Scheduler shows the task not running
/// and the daemon's address is free. `/End` returns before the server has exited (on a
/// hosted runner the old server was still answering, and a `/Run` right after it failed
/// to bind), so the task's state alone does not prove the server is gone. Neither does a
/// failed `GET /health`: a hung server times out or errors while it still holds the port
/// (review round 3). Returns the last task state and whether the address was still taken.
fn wait_task_down(label: &str, bind: &str) -> (TaskRun, bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let state = task_run_state(label);
        let taken = address_taken(bind);
        let down = !taken && matches!(state, TaskRun::Stopped | TaskRun::Absent);
        if down || std::time::Instant::now() >= deadline {
            return (state, taken);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Whether something still holds the daemon's address. The only positive proof that the
/// old server let go is binding the address ourselves (and closing it at once), so any
/// failure, including an address that does not resolve, counts as taken.
fn address_taken(bind: &str) -> bool {
    use std::net::ToSocketAddrs;
    let Ok(addrs) = bind.to_socket_addrs() else {
        return true;
    };
    let addrs: Vec<_> = addrs.collect();
    addrs.is_empty()
        || addrs
            .iter()
            .any(|addr| std::net::TcpListener::bind(addr).is_err())
}

/// The fields of one CSV line as `schtasks /FO CSV` writes it: every field quoted, `""`
/// for a quote inside a field.
fn csv_fields(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = line.trim_end_matches(['\r', '\n']).chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            ('"', _) => quoted = !quoted,
            (',', false) => fields.push(std::mem::take(&mut field)),
            (c, _) => field.push(c),
        }
    }
    fields.push(field);
    fields
}

/// A Task Scheduler task, from the full task list (`schtasks /Query /FO CSV /NH`). Task
/// names are not translated, so this does not depend on the system language, and a failed
/// listing is `Unknown` rather than "not registered".
fn task_presence(label: &str) -> Presence {
    match exec(&argv(&["schtasks", "/Query", "/FO", "CSV", "/NH"])) {
        Ok(out) if out.status.success() => {
            let want = format!("\"\\{label}\"");
            let listed = String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|l| l.split(',').next().map(str::trim) == Some(want.as_str()));
            if listed {
                Presence::Present
            } else {
                Presence::Absent
            }
        }
        Ok(out) => Presence::Unknown(stderr_of(&out)),
        Err(e) => Presence::Unknown(format!("{e:#}")),
    }
}

/// Restart the running service.
pub fn restart(env: &DaemonEnv) -> Result<()> {
    validate_label(&env.label)?;
    match env.manager {
        Manager::Launchd => {
            let target = launchd_target(&uid()?, &env.label);
            let out = exec(&argv(&["launchctl", "kickstart", "-k", &target]))?;
            if !out.status.success() {
                bail!(
                    "launchctl kickstart failed (is the daemon installed?): {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
        }
        Manager::Systemd => {
            must(&argv(&["systemctl", "--user", "restart", &env.unit_name()]))?;
        }
        Manager::TaskScheduler => {
            // `/End` fails for a task that is not running, so its exit status is not used.
            // `/Run` waits until the old server is gone, or the new one cannot bind.
            let bind = read_definition(env.manager, &env.file_path())
                .and_then(|t| bind_in(env.manager, &t))
                .unwrap_or_else(|| env.bind.clone());
            let ended = exec(&argv(&["schtasks", "/End", "/TN", &env.label]))?;
            // `/Run` only after the task is shown stopped (or gone) and the address is
            // free; an unreadable state is not a stop (review round 3).
            let (state, taken) = wait_task_down(&env.label, &bind);
            if let TaskRun::Unknown(why) = &state {
                bail!(
                    "could not confirm that the old {} server stopped (`schtasks /End` said {}; \
                     reading the task's state failed: {why}), so it was not started again; run \
                     `another daemon restart` again once Task Scheduler answers",
                    env.label,
                    stderr_of(&ended)
                );
            }
            if state == TaskRun::Running || taken {
                bail!(
                    "the old {} server did not stop within 10 seconds (`schtasks /End` said {}; \
                     task {}; {bind} {}), so it was not started again",
                    env.label,
                    stderr_of(&ended),
                    if state == TaskRun::Running {
                        "still running"
                    } else {
                        "not running"
                    },
                    if taken { "is still in use" } else { "is free" }
                );
            }
            must(&argv(&["schtasks", "/Run", "/TN", &env.label]))?;
        }
    }
    Ok(())
}

fn healthy(bind: &str, local_node: &str) -> Result<()> {
    if !bind
        .parse::<std::net::SocketAddr>()
        .is_ok_and(|address| address.ip().is_loopback())
    {
        bail!("{bind} is not a numeric loopback address; /health was not checked");
    }
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(2)))
            .build(),
    );
    let mut response = agent
        .get(&format!("http://{bind}/health"))
        .call()
        .with_context(|| format!("could not reach /health on {bind}"))?;
    if response.status().is_success()
        && response
            .body_mut()
            .with_config()
            .limit(16 * 1024)
            .read_to_string()
            .ok()
            .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
            .is_some_and(|body| body["node_id"].as_str() == Some(local_node))
    {
        Ok(())
    } else {
        bail!("/health on {bind} did not identify this home's node {local_node}")
    }
}

/// `loaded` and `pid` from `systemctl --user show --property=...` output.
fn systemd_state(show: &str) -> (bool, Option<u32>) {
    let value = |key: &str| {
        show.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .map(str::trim)
    };
    let loaded = value("LoadState") == Some("loaded")
        && matches!(value("UnitFileState"), Some("enabled" | "enabled-runtime"));
    let pid = value("MainPID")
        .and_then(|p| p.parse::<u32>().ok())
        .filter(|p| *p > 0);
    (loaded, pid)
}

/// The quoted Environment line emitted by `another daemon install`, reversing its
/// backslash escapes and doubled literal percent signs, not expanding specifiers.
pub(crate) fn systemd_home(unit: &str) -> Option<PathBuf> {
    let encoded = unit
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("Environment=\"ANOTHER_HOME="))?
        .strip_suffix('"')?;
    let mut value = String::new();
    let mut chars = encoded.chars();
    while let Some(c) = chars.next() {
        value.push(match c {
            '\\' => match chars.next()? {
                c @ ('\\' | '"') => c,
                _ => return None,
            },
            '%' if chars.next()? == '%' => '%',
            '%' | '"' => return None,
            c => c,
        });
    }
    (!value.is_empty()).then(|| PathBuf::from(value))
}

fn same_home(recorded: &Path, home: &Path) -> bool {
    match (recorded.canonicalize(), home.canonicalize()) {
        (Ok(recorded), Ok(home)) => recorded == home,
        _ => recorded == home,
    }
}

/// Is the service installed, loaded, running and answering?
pub fn status(env: &DaemonEnv) -> Result<DaemonStatus> {
    let mut s = service(env)?;
    if s.detail.is_none() {
        if let Some(bind) = &s.bind {
            match another_core::Store::open_read_only(&env.another_home, Duration::from_secs(2)) {
                Ok(store) => {
                    s.detail = healthy(bind, store.node_id()).err().map(|e| e.to_string());
                    s.healthy = s.detail.is_none();
                }
                Err(e) => s.detail = Some(format!("could not read this home's node: {e}")),
            }
        }
    }
    Ok(s)
}

fn launchd_state(text: &str, home: &Path) -> (bool, Option<u32>, Option<String>) {
    let recorded_home = text
        .lines()
        .map(str::trim)
        .skip_while(|line| *line != "environment = {")
        .skip(1)
        .take_while(|line| *line != "}")
        .find_map(|line| line.strip_prefix("ANOTHER_HOME => "));
    let Some(recorded_home) = recorded_home.filter(|h| !h.is_empty()) else {
        return (
            false,
            None,
            Some("loaded launchd label's ANOTHER_HOME could not be verified".into()),
        );
    };
    let recorded = Path::new(recorded_home);
    if !same_home(recorded, home) {
        return (
            false,
            None,
            Some(format!(
                "loaded launchd label belongs to a different home: {recorded_home:?}"
            )),
        );
    }
    let pid = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("pid = "))
        .and_then(|p| p.trim().parse().ok());
    (true, pid, None)
}

/// What the service manager and the service's file say, with no network call: `healthy`
/// is always false here. `another doctor` uses this and makes its own single `/health` call.
pub fn service(env: &DaemonEnv) -> Result<DaemonStatus> {
    validate_label(&env.label)?;
    let path = env.file_path();
    let installed_text = read_definition(env.manager, &path);
    let mut detail = None;
    let (mut loaded, mut pid) = match env.manager {
        Manager::Launchd => {
            let out = exec(&argv(&[
                "launchctl",
                "print",
                &launchd_target(&uid()?, &env.label),
            ]))?;
            let text = String::from_utf8_lossy(&out.stdout);
            if out.status.success() {
                let (loaded, pid, reason) = launchd_state(&text, &env.another_home);
                detail = reason;
                (loaded, pid)
            } else {
                (false, None)
            }
        }
        Manager::Systemd => {
            let unit = env.unit_name();
            match exec(&argv(&[
                "systemctl",
                "--user",
                "show",
                &unit,
                "--property=LoadState,UnitFileState,ActiveState,MainPID",
            ])) {
                Ok(out) if out.status.success() => {
                    systemd_state(&String::from_utf8_lossy(&out.stdout))
                }
                // No reachable user manager: nothing is loaded from this session's view.
                _ => (false, None),
            }
        }
        Manager::TaskScheduler => (task_presence(&env.label) == Presence::Present, None),
    };
    if env.manager == Manager::Systemd && (installed_text.is_some() || loaded || pid.is_some()) {
        detail = match installed_text.as_deref().and_then(systemd_home) {
            Some(home) if same_home(&home, &env.another_home) => None,
            Some(home) => Some(format!(
                "systemd unit belongs to a different home: {:?}",
                home.display().to_string()
            )),
            None => Some("systemd unit's ANOTHER_HOME could not be verified".into()),
        };
        if detail.is_some() {
            loaded = false;
            pid = None;
        }
    }
    let bind = installed_text
        .as_deref()
        .and_then(|t| bind_in(env.manager, t));
    Ok(DaemonStatus {
        manager: env.manager,
        label: env.label.clone(),
        file: path.display().to_string(),
        plist: macos_plist_key(env),
        installed: installed_text.is_some(),
        loaded,
        detail,
        pid,
        bind,
        healthy: false,
        log: env.log_path().display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn health_fixture(body: &str) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let bind = listener.local_addr().unwrap().to_string();
        let body = body.to_string();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 1024];
            assert!(stream.read(&mut request).unwrap() > 0);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        (bind, server)
    }

    #[test]
    fn health_requires_this_homes_node() {
        let (bind, server) = health_fixture(r#"{"ok":true,"node_id":"other-node"}"#);
        let result = healthy(&bind, "this-node").is_ok();
        server.join().unwrap();
        assert!(
            !result,
            "a different node must not be healthy for this home"
        );
    }

    #[test]
    fn health_requires_a_matching_node_in_valid_json() {
        for (body, expected) in [
            (r#"{"ok":true,"node_id":"this-node"}"#, true),
            (r#"{"ok":true}"#, false),
            ("not json", false),
        ] {
            let (bind, server) = health_fixture(body);
            let result = healthy(&bind, "this-node").is_ok();
            server.join().unwrap();
            assert_eq!(result, expected, "{body}");
        }
    }

    #[test]
    fn health_reports_a_closed_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let bind = listener.local_addr().unwrap().to_string();
        drop(listener);
        let error = healthy(&bind, "this-node").unwrap_err().to_string();
        assert_eq!(error, format!("could not reach /health on {bind}"));
    }

    #[test]
    fn health_rejects_a_non_numeric_bind() {
        let error = healthy("localhost:7466", "this-node")
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "localhost:7466 is not a numeric loopback address; /health was not checked"
        );
    }

    #[test]
    fn launchd_status_rejects_another_home() {
        let text =
            "job = {\n environment = {\n ANOTHER_HOME => /work/other home\n }\n pid = 4242\n}\n";
        let (loaded, pid, detail) = launchd_state(text, Path::new("/work/current"));
        assert!(!loaded, "another home's daemon is not loaded for this home");
        assert_eq!(pid, None);
        assert!(detail.unwrap().contains("different home"));
    }

    #[test]
    fn launchd_status_accepts_current_home() {
        let text =
            "job = {\n environment = {\n ANOTHER_HOME => /work/current home\n }\n pid = 4242\n}\n";
        assert_eq!(
            launchd_state(text, Path::new("/work/current home")),
            (true, Some(4242), None)
        );
        assert_eq!(
            launchd_state(
                &text.replace(" pid = 4242\n", ""),
                Path::new("/work/current home")
            ),
            (true, None, None)
        );
    }

    #[test]
    fn launchd_status_requires_recorded_home() {
        let text = "job = {\n default environment = {\n ANOTHER_HOME => /work/current\n }\n environment = {\n PATH => /bin\n }\n pid = 4242\n}\n";
        let (loaded, pid, detail) = launchd_state(text, Path::new("/work/current"));
        assert!(!loaded);
        assert_eq!(pid, None);
        assert!(detail.unwrap().contains("could not be verified"));
    }

    #[cfg(unix)]
    #[test]
    fn launchd_status_resolves_home_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let alias = dir.path().join("alias");
        std::fs::create_dir(&home).unwrap();
        std::os::unix::fs::symlink(&home, &alias).unwrap();
        let text = format!(
            "environment = {{\n ANOTHER_HOME => {}\n}}\npid = 4242\n",
            home.display()
        );
        assert_eq!(launchd_state(&text, &alias), (true, Some(4242), None));
    }

    fn env(manager: Manager) -> DaemonEnv {
        DaemonEnv {
            manager,
            home: "/Users/someone".into(),
            config_home: None,
            another_home: "/Users/someone/.another".into(),
            exe: "/opt/another & co/another".into(),
            path: Some("/usr/bin:/bin:/opt/homebrew/bin".into()),
            user: Some("HOST\\someone".into()),
            label: DEFAULT_LABEL.into(),
            bind: "127.0.0.1:7466".into(),
        }
    }

    #[test]
    fn plist_runs_serve_on_loopback_with_keepalive() {
        let e = env(Manager::Launchd);
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
        assert_eq!(
            bind_in(Manager::Launchd, &text).as_deref(),
            Some("127.0.0.1:7466")
        );
        assert_eq!(
            e.file_path(),
            Path::new("/Users/someone")
                .join("Library")
                .join("LaunchAgents")
                .join("one.toanother.another.plist")
        );
    }

    #[test]
    fn systemd_unit_quotes_every_value_literally() {
        let mut e = env(Manager::Systemd);
        e.exe = "/opt/my \"odd\" $dir/50%/another".into();
        e.another_home = "/home/someone/data 100%".into();
        e.path = Some("/usr/bin:/home/someone/$bin".into());
        let text = systemd_unit(&e).unwrap();
        // Review P2-2: systemd resolves the executable path before it expands `$` in argv,
        // so a `$` in the path stays single; `$$` would name a different file.
        assert!(
            text.contains(
                "ExecStart=\"/opt/my \\\"odd\\\" $dir/50%%/another\" serve --bind 127.0.0.1:7466\n"
            ),
            "{text}"
        );
        assert!(text.contains("Environment=\"ANOTHER_HOME=/home/someone/data 100%%\"\n"));
        // Environment= does not expand `$`, so it stays single.
        assert!(text.contains("Environment=\"PATH=/usr/bin:/home/someone/$bin\"\n"));
        let log = Path::new("/home/someone/data 100%")
            .join("logs")
            .join("serve.log")
            .display()
            .to_string()
            .replace('%', "%%");
        assert!(text.contains(&format!("StandardOutput=append:{log}\n")));
        assert!(text.contains("Restart=always\n"));
        assert!(text.contains("WantedBy=default.target\n"));
        assert_eq!(
            bind_in(Manager::Systemd, &text).as_deref(),
            Some("127.0.0.1:7466")
        );

        e.another_home = "/home/someone/line\nbreak".into();
        assert!(systemd_unit(&e).is_err(), "control characters are refused");
    }

    #[test]
    fn systemd_units_live_in_the_user_config_folder() {
        let mut e = env(Manager::Systemd);
        e.home = "/home/someone".into();
        assert_eq!(
            e.file_path(),
            Path::new("/home/someone/.config/systemd/user/one.toanother.another.service")
        );
        e.config_home = Some("/xdg".into());
        assert_eq!(
            e.file_path(),
            Path::new("/xdg/systemd/user/one.toanother.another.service")
        );
    }

    #[test]
    fn systemctl_show_parses_into_status() {
        let up = "LoadState=loaded\nUnitFileState=enabled\nActiveState=active\nMainPID=4242\n";
        assert_eq!(systemd_state(up), (true, Some(4242)));
        let stopped = "LoadState=loaded\nUnitFileState=enabled\nActiveState=inactive\nMainPID=0\n";
        assert_eq!(systemd_state(stopped), (true, None));
        let disabled = "LoadState=loaded\nUnitFileState=disabled\nActiveState=active\nMainPID=7\n";
        assert_eq!(systemd_state(disabled), (false, Some(7)));
        let missing = "LoadState=not-found\nUnitFileState=\nActiveState=inactive\nMainPID=0\n";
        assert_eq!(systemd_state(missing), (false, None));
    }

    #[test]
    fn task_xml_starts_serve_at_logon_as_this_user() {
        let mut e = env(Manager::TaskScheduler);
        e.exe = r"C:\Program Files\Another & Co\another.exe".into();
        e.another_home = r"C:\Users\someone\.another\".into();
        let text = task_xml(&e).unwrap();
        assert!(text.contains("<UserId>HOST\\someone</UserId>"));
        assert!(text.contains("<LogonTrigger>"));
        assert!(text.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(text.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(text.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(text.contains(r"<Command>C:\Program Files\Another &amp; Co\another.exe</Command>"));
        // A trailing backslash is doubled so it cannot escape the closing quote.
        assert!(
            text.contains(r#"<Arguments>--home &quot;C:\Users\someone\.another\\&quot; serve --bind 127.0.0.1:7466 --log-file &quot;"#),
            "{text}"
        );
        assert_eq!(
            bind_in(Manager::TaskScheduler, &text).as_deref(),
            Some("127.0.0.1:7466")
        );
        let bytes = utf16(&text);
        assert_eq!(&bytes[..2], &[0xFF, 0xFE]);
        assert_eq!(from_utf16(&bytes).as_deref(), Some(text.as_str()));

        e.user = None;
        assert!(task_xml(&e).is_err(), "the logon task needs a user");
        e.user = Some("HOST\\someone".into());
        e.exe = "C:\\odd\"name\\another.exe".into();
        assert!(task_xml(&e).is_err(), "a double quote cannot be recorded");
    }

    #[test]
    fn schtasks_csv_lines_split_like_schtasks_writes_them() {
        let line = "\"HOST\",\"\\one.toanother.another\",\"N/A\",\"Running\",\"Interactive only\",\"11/16/2026, 9:00:00 AM\",\"267009\",\"HOST\\\\me\",\"C:\\\\x \"\"quoted\"\" y\"\r";
        let f = csv_fields(line);
        assert_eq!(f[1], "\\one.toanother.another");
        assert_eq!(f[5], "11/16/2026, 9:00:00 AM", "a comma inside quotes");
        assert_eq!(f[6], SCHED_S_TASK_RUNNING);
        assert_eq!(f[8], "C:\\\\x \"quoted\" y", "a doubled quote is one quote");
        assert_eq!(f.len(), 9);
    }

    #[test]
    fn windows_quote_keeps_backslashes_literal() {
        assert_eq!(windows_quote(r"C:\a b"), r#""C:\a b""#);
        assert_eq!(windows_quote(r"C:\a\"), r#""C:\a\\""#);
        assert_eq!(windows_quote(r"C:\a\\"), r#""C:\a\\\\""#);
    }

    #[test]
    fn labels_are_checked() {
        assert!(validate_label(DEFAULT_LABEL).is_ok());
        assert!(validate_label("one.toanother.another-test_2").is_ok());
        assert!(validate_label("../evil").is_err());
        assert!(validate_label("").is_err());
    }

    // Temp names are unique per write so low-level writers do not collide on temp paths;
    // concurrent daemon installs are not supported.
    #[test]
    fn write_definition_handles_concurrent_writers() {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("one.toanother.another.plist");

        let threads = 8;
        let iters = 40;
        let barrier = Arc::new(Barrier::new(threads));
        let mut handles = Vec::with_capacity(threads);
        for t in 0..threads {
            let barrier = Arc::clone(&barrier);
            let path = path.clone();
            handles.push(std::thread::spawn(move || {
                let pad = "a".repeat(8192);
                barrier.wait();
                let mut errors = Vec::new();
                for i in 0..iters {
                    let payload = format!("<plist t=\"{t}\" i=\"{i}\">{pad}</plist>");
                    if let Err(err) = write_definition(&path, payload.as_bytes()) {
                        errors.push(format!("t={t} i={i}: {err:#}"));
                    }
                }
                errors
            }));
        }

        let mut all_errors = Vec::new();
        for h in handles {
            all_errors.extend(h.join().unwrap());
        }
        assert!(
            all_errors.is_empty(),
            "concurrent write_definition failed ({} errors), first: {}",
            all_errors.len(),
            all_errors.first().map(String::as_str).unwrap_or("")
        );

        let final_text = std::fs::read_to_string(&path).unwrap();
        assert!(final_text.starts_with("<plist "));
        assert!(final_text.ends_with("</plist>"));
        assert!(final_text.contains(&"a".repeat(8192)));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o644
            );
        }

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("another-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
    }

    #[test]
    fn write_definition_cleans_up_temp_file_on_error() {
        let dir = tempfile::tempdir().unwrap();
        // A directory at the target path makes the final rename fail after the temp file is created.
        let target = dir.path().join("one.toanother.another.plist");
        std::fs::create_dir(&target).unwrap();

        assert!(write_definition(&target, b"<plist/>").is_err());

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("another-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
    }

    #[test]
    fn write_definition_sweeps_stale_temp_siblings_and_leaves_unrelated_files() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("one.toanother.another.plist");

        let stale_a = dir
            .path()
            .join("one.toanother.another.plist.another-tmp.99999.0.deadbeef");
        let stale_b = dir
            .path()
            .join("one.toanother.another.plist.another-tmp.88888.12.cafebabe");
        std::fs::write(&stale_a, "crashed-1").unwrap();
        std::fs::write(&stale_b, "crashed-2").unwrap();

        let other_target_tmp = dir.path().join("other.plist.another-tmp.99999.0.deadbeef");
        let malformed_backup = dir
            .path()
            .join("one.toanother.another.plist.another-tmp.backup");
        let malformed_stale = dir
            .path()
            .join("one.toanother.another.plist.another-tmp.stale");
        let bare_suffix = dir.path().join("one.toanother.another.plist.another-tmp");
        let old_ext = dir.path().join("one.toanother.another.another-tmp");
        let unrelated = dir.path().join("notes.txt");
        for p in [
            &other_target_tmp,
            &malformed_backup,
            &malformed_stale,
            &bare_suffix,
            &old_ext,
            &unrelated,
        ] {
            std::fs::write(p, "keep").unwrap();
        }

        let stale_dir = dir
            .path()
            .join("one.toanother.another.plist.another-tmp.77777.0.feedface");
        std::fs::create_dir(&stale_dir).unwrap();

        #[cfg(unix)]
        let (victim, stale_link) = {
            let victim = dir.path().join("victim.txt");
            std::fs::write(&victim, "do-not-touch").unwrap();
            let link = dir
                .path()
                .join("one.toanother.another.plist.another-tmp.66666.0.badc0ffe");
            std::os::unix::fs::symlink(&victim, &link).unwrap();
            (victim, link)
        };

        write_definition(&target, b"<plist/>").unwrap();

        assert!(!stale_a.exists(), "stale sibling temp file should be swept");
        assert!(!stale_b.exists(), "stale sibling temp file should be swept");
        for p in [
            &other_target_tmp,
            &malformed_backup,
            &malformed_stale,
            &bare_suffix,
            &old_ext,
            &unrelated,
        ] {
            assert!(p.exists(), "unrelated file must remain: {}", p.display());
        }
        assert!(stale_dir.is_dir(), "directory matching prefix must remain");

        #[cfg(unix)]
        {
            assert!(
                std::fs::symlink_metadata(&stale_link)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "symlink matching prefix must not be followed or removed"
            );
            assert_eq!(std::fs::read_to_string(&victim).unwrap(), "do-not-touch");
        }
    }

    #[test]
    fn remove_sweeps_stale_temp_siblings_even_when_target_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("one.toanother.another.service");
        let stale = dir
            .path()
            .join("one.toanother.another.service.another-tmp.99999.0.deadbeef");
        let other = dir
            .path()
            .join("other.service.another-tmp.99999.0.deadbeef");
        let keep = dir.path().join("unrelated.txt");
        std::fs::write(&stale, "crashed").unwrap();
        std::fs::write(&other, "other").unwrap();
        std::fs::write(&keep, "keep").unwrap();

        remove(&target, false).unwrap();

        assert!(!stale.exists(), "stale sibling temp file should be swept");
        assert!(other.exists(), "sibling for another target must remain");
        assert!(keep.exists(), "unrelated file must remain");
    }

    #[test]
    fn write_definition_handles_250_byte_target_name() {
        use std::ffi::OsStr;

        let dir = tempfile::tempdir().unwrap();
        let long_name = format!("{}.service", "d".repeat(242));
        let other_long_name = format!("{}e.service", "d".repeat(241));
        assert_eq!(long_name.len(), 250);
        assert_eq!(other_long_name.len(), 250);
        let target = dir.path().join(&long_name);

        let mut stale_name = crate::atomic_write::temp_prefix(OsStr::new(&long_name));
        stale_name.push("99999.0.deadbeef");
        let stale = dir.path().join(&stale_name);

        let mut other_stale_name = crate::atomic_write::temp_prefix(OsStr::new(&other_long_name));
        other_stale_name.push("99999.0.deadbeef");
        let other_stale = dir.path().join(&other_stale_name);

        let unrelated = dir.path().join("keep.txt");
        for p in [&stale, &other_stale, &unrelated] {
            std::fs::write(p, "data").unwrap();
        }

        write_definition(&target, b"[Unit]\nDescription=another\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "[Unit]\nDescription=another\n"
        );
        assert!(
            !stale.exists(),
            "stale temp for 250-byte target should be swept"
        );
        assert!(
            other_stale.exists(),
            "stale temp for distinct 250-byte target must remain"
        );
        assert!(unrelated.exists(), "unrelated file must remain");

        // Uninstall (`remove`) also sweeps stale temps for 250-byte names without touching others.
        std::fs::write(&stale, "crashed-again").unwrap();
        remove(&target, true).unwrap();
        assert!(!target.exists());
        assert!(!stale.exists(), "remove must sweep 250-byte stale temp");
        assert!(other_stale.exists(), "distinct 250-byte temp must remain");
        assert!(unrelated.exists(), "unrelated file must remain");
    }
}
