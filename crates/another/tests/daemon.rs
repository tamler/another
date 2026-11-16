//! `another daemon`: the per-user service definition on each platform, written into a
//! temporary HOME. Nothing here loads a real service on a developer machine: macOS and
//! Linux tests never reach launchd or a real systemd (Linux uses a fake `systemctl` first on
//! PATH), and the tests that drive a real Task Scheduler or systemd user manager run only on
//! GitHub-hosted runners.

#[allow(dead_code)]
#[path = "common/hosted_ci.rs"]
mod hosted_ci;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Env {
    _dir: tempfile::TempDir,
    home: PathBuf,
    /// Extra directory put first on PATH (fake service managers).
    bin: PathBuf,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        Env {
            home: dir.path().join("home"),
            bin,
            _dir: dir,
        }
    }

    fn another_home(&self) -> PathBuf {
        self.home.join(".another")
    }

    fn cmd(&self, args: &[&str]) -> Command {
        self.cmd_with(Path::new(env!("CARGO_BIN_EXE_another")), args)
    }

    /// Run a given `another` binary, for example a copy at an unusual path.
    fn cmd_with(&self, exe: &Path, args: &[&str]) -> Command {
        let mut c = Command::new(exe);
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut dirs = vec![self.bin.clone()];
        dirs.extend(std::env::split_paths(&path));
        c.args(args)
            .env("HOME", &self.home)
            .env("ANOTHER_HOME", self.another_home())
            .env("PATH", std::env::join_paths(dirs).unwrap())
            .env_remove("XDG_CONFIG_HOME")
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let out = self.run(&all);
        assert!(
            out.status.success(),
            "another {all:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

/// A label of its own per test, so nothing here can touch a real daemon.
fn test_label(tag: &str) -> String {
    format!("one.toanother.another.test-{tag}-{}", std::process::id())
}

#[cfg(target_os = "macos")]
#[test]
fn install_writes_a_loopback_keepalive_launch_agent() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    let label = test_label("launchd");
    let plist = env
        .home
        .join("Library/LaunchAgents")
        .join(format!("{label}.plist"));

    let dry = env.json(&[
        "daemon",
        "install",
        "--label",
        &label,
        "--bind",
        "127.0.0.1:7499",
        "--dry-run",
    ]);
    assert_eq!(dry["change"], "create");
    assert_eq!(dry["manager"], "launchd");
    assert_eq!(dry["file"], plist.display().to_string());
    assert_eq!(
        dry["plist"],
        plist.display().to_string(),
        "kept for scripts"
    );
    assert_eq!(dry["commands"][1][1], "bootstrap");
    assert!(!plist.exists(), "dry run writes nothing");

    env.json(&[
        "daemon",
        "install",
        "--label",
        &label,
        "--bind",
        "127.0.0.1:7499",
        "--no-load",
    ]);
    let text = std::fs::read_to_string(&plist).unwrap();
    assert!(text.contains(&format!("<string>{label}</string>")));
    assert!(text.contains("<string>serve</string>"));
    assert!(text.contains("<string>127.0.0.1:7499</string>"));
    assert!(text.contains("<key>KeepAlive</key>\n  <true/>"));
    assert!(text.contains("<key>ANOTHER_HOME</key>"));
    assert!(text.contains(".another/logs/serve.log"));
    assert_eq!(
        std::fs::metadata(&plist).unwrap().permissions().mode() & 0o777,
        0o644
    );
    assert!(env.home.join(".another/logs").is_dir());

    let again = env.json(&[
        "daemon",
        "install",
        "--label",
        &label,
        "--bind",
        "127.0.0.1:7499",
        "--no-load",
    ]);
    assert_eq!(again["change"], "unchanged");

    // Loopback only.
    let out = env.run(&[
        "daemon",
        "install",
        "--label",
        &label,
        "--bind",
        "0.0.0.0:7499",
        "--no-load",
    ]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("loopback"));

    // Uninstall removes the plist (unloading a label launchd never saw is harmless).
    let dry = env.json(&["daemon", "uninstall", "--label", &label, "--dry-run"]);
    assert_eq!(
        dry["bind"], "127.0.0.1:7499",
        "reports the installed address"
    );
    assert!(plist.exists());
    let out = env.run(&["daemon", "uninstall", "--label", &label]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!plist.exists());
}

/// A fake `systemctl` that records every call and answers `show` from a state file.
#[cfg(target_os = "linux")]
fn fake_systemctl(env: &Env, fail_with: Option<&str>) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let log = env.bin.join("systemctl.log");
    let script = match fail_with {
        Some(message) => format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\necho '{message}' >&2\nexit 1\n",
            log = log.display()
        ),
        None => format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\n\
             case \"$2\" in\n  show) printf 'LoadState=loaded\\nUnitFileState=enabled\\nActiveState=active\\nMainPID=4242\\n' ;;\nesac\nexit 0\n",
            log = log.display()
        ),
    };
    let path = env.bin.join("systemctl");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    log
}

#[cfg(target_os = "linux")]
fn calls(log: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[cfg(target_os = "linux")]
#[test]
fn linux_install_writes_and_drives_a_systemd_user_unit() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    let log = fake_systemctl(&env, None);
    let label = test_label("systemd");
    let unit_name = format!("{label}.service");
    let unit = env.home.join(".config/systemd/user").join(&unit_name);
    let bind = "127.0.0.1:7499";

    let dry = env.json(&[
        "daemon",
        "install",
        "--label",
        &label,
        "--bind",
        bind,
        "--dry-run",
    ]);
    assert_eq!(dry["manager"], "systemd");
    assert_eq!(dry["change"], "create");
    assert_eq!(dry["file"], unit.display().to_string());
    assert_eq!(
        dry["commands"],
        serde_json::json!([
            ["systemctl", "--user", "daemon-reload"],
            ["systemctl", "--user", "enable", unit_name],
            ["systemctl", "--user", "restart", unit_name],
        ])
    );
    assert!(!unit.exists(), "dry run writes nothing");
    assert!(calls(&log).is_empty(), "dry run runs nothing");

    env.json(&[
        "daemon",
        "install",
        "--label",
        &label,
        "--bind",
        bind,
        "--no-load",
    ]);
    assert!(calls(&log).is_empty(), "--no-load runs nothing");
    let text = std::fs::read_to_string(&unit).unwrap();
    let exe = env!("CARGO_BIN_EXE_another");
    assert!(
        text.contains(&format!("ExecStart=\"{exe}\" serve --bind {bind}\n")),
        "{text}"
    );
    assert!(text.contains(&format!(
        "Environment=\"ANOTHER_HOME={}\"\n",
        env.another_home().display()
    )));
    assert!(text.contains("Environment=\"PATH="));
    assert!(text.contains("Restart=always\n"));
    assert!(text.contains("RestartSec=10\n"));
    let serve_log = env.another_home().join("logs/serve.log");
    assert!(text.contains(&format!("StandardOutput=append:{}\n", serve_log.display())));
    assert!(text.contains(&format!("StandardError=append:{}\n", serve_log.display())));
    assert!(text.contains("WantedBy=default.target\n"));
    assert_eq!(
        std::fs::metadata(&unit).unwrap().permissions().mode() & 0o777,
        0o644
    );
    assert!(env.another_home().join("logs").is_dir());

    let again = env.json(&[
        "daemon",
        "install",
        "--label",
        &label,
        "--bind",
        bind,
        "--no-load",
    ]);
    assert_eq!(again["change"], "unchanged");

    let out = env.run(&[
        "daemon",
        "install",
        "--label",
        &label,
        "--bind",
        "0.0.0.0:7499",
        "--no-load",
    ]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("loopback"));

    // Load it: reload, enable, (re)start, in that order.
    env.json(&["daemon", "install", "--label", &label, "--bind", bind]);
    assert_eq!(
        calls(&log),
        [
            "--user daemon-reload".to_string(),
            format!("--user enable {unit_name}"),
            format!("--user restart {unit_name}"),
        ]
    );

    let status = env.json(&["daemon", "status", "--label", &label]);
    assert_eq!(status["manager"], "systemd");
    assert_eq!(status["installed"], true);
    assert_eq!(status["loaded"], true);
    assert_eq!(status["pid"], 4242);
    assert_eq!(status["bind"], bind);
    assert_eq!(status["file"], unit.display().to_string());
    assert!(calls(&log)
        .last()
        .unwrap()
        .starts_with(&format!("--user show {unit_name}")));

    env.json(&["daemon", "restart", "--label", &label]);
    assert_eq!(
        calls(&log).last().unwrap(),
        &format!("--user restart {unit_name}")
    );

    let dry = env.json(&["daemon", "uninstall", "--label", &label, "--dry-run"]);
    assert_eq!(dry["bind"], bind, "reports the installed address");
    assert!(unit.exists());
    let before = calls(&log).len();
    env.json(&["daemon", "uninstall", "--label", &label]);
    assert!(!unit.exists());
    assert_eq!(
        calls(&log)[before..],
        [
            format!("--user disable --now {unit_name}"),
            "--user daemon-reload".to_string(),
        ]
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_unit_follows_xdg_config_home() {
    let env = Env::new();
    fake_systemctl(&env, None);
    let label = test_label("xdg");
    let config = env.home.join("elsewhere");
    let out = env
        .cmd(&[
            "--json",
            "daemon",
            "install",
            "--label",
            &label,
            "--no-load",
        ])
        .env("XDG_CONFIG_HOME", &config)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(config
        .join("systemd/user")
        .join(format!("{label}.service"))
        .is_file());
}

#[cfg(target_os = "linux")]
#[test]
fn linux_install_explains_an_unreachable_user_manager() {
    let env = Env::new();
    fake_systemctl(&env, Some("Failed to connect to bus: No medium found"));
    let label = test_label("nobus");
    let out = env.run(&["daemon", "install", "--label", &label]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("Failed to connect to bus"), "{err}");
    assert!(err.contains("systemd user manager"), "{err}");
    assert!(err.contains("loginctl enable-linger"), "{err}");
}

/// Decode the UTF-16 task definition written for Task Scheduler.
#[cfg(windows)]
fn read_task(path: &std::path::Path) -> String {
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(
        &bytes[..2],
        &[0xFF, 0xFE],
        "UTF-16 LE with a byte order mark"
    );
    let units: Vec<u16> = bytes[2..]
        .chunks(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16(&units).unwrap()
}

#[cfg(windows)]
#[test]
fn windows_install_writes_a_per_user_logon_task() {
    let env = Env::new();
    let label = test_label("tasks");
    let task = env
        .another_home()
        .join("daemon")
        .join(format!("{label}.xml"));
    let bind = "127.0.0.1:7499";

    let dry = env.json(&[
        "daemon",
        "install",
        "--label",
        &label,
        "--bind",
        bind,
        "--dry-run",
    ]);
    assert_eq!(dry["manager"], "task_scheduler");
    assert_eq!(dry["change"], "create");
    assert_eq!(dry["file"], task.display().to_string());
    assert_eq!(
        dry["commands"],
        serde_json::json!([
            [
                "schtasks",
                "/Create",
                "/TN",
                label,
                "/XML",
                task.display().to_string(),
                "/F"
            ],
            ["schtasks", "/Run", "/TN", label],
        ])
    );
    assert!(!task.exists(), "dry run writes nothing");

    env.json(&[
        "daemon",
        "install",
        "--label",
        &label,
        "--bind",
        bind,
        "--no-load",
    ]);
    let text = read_task(&task);
    let exe = env!("CARGO_BIN_EXE_another");
    assert!(
        text.contains(&format!("<Command>{exe}</Command>")),
        "{text}"
    );
    let home = env.another_home().display().to_string();
    let log = env
        .another_home()
        .join("logs")
        .join("serve.log")
        .display()
        .to_string();
    assert!(
        text.contains(&format!(
            "<Arguments>--home &quot;{home}&quot; serve --bind {bind} --log-file &quot;{log}&quot;</Arguments>"
        )),
        "{text}"
    );
    assert!(text.contains("<LogonTrigger>"));
    assert!(text.contains("<LogonType>InteractiveToken</LogonType>"));
    assert!(text.contains("<RunLevel>LeastPrivilege</RunLevel>"));
    assert!(text.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
    assert!(env.another_home().join("logs").is_dir());

    let again = env.json(&[
        "daemon",
        "install",
        "--label",
        &label,
        "--bind",
        bind,
        "--no-load",
    ]);
    assert_eq!(again["change"], "unchanged");

    let out = env.run(&[
        "daemon",
        "install",
        "--label",
        &label,
        "--bind",
        "0.0.0.0:7499",
        "--no-load",
    ]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("loopback"));

    let dry = env.json(&["daemon", "uninstall", "--label", &label, "--dry-run"]);
    assert_eq!(dry["bind"], bind, "reports the installed address");
    assert!(task.exists());
    // The task was never registered: ending and deleting it fail, which is fine.
    env.json(&["daemon", "uninstall", "--label", &label]);
    assert!(!task.exists());
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
#[test]
fn install_explains_the_supported_platforms() {
    let env = Env::new();
    let out = env.run(&["daemon", "install", "--no-load"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("macOS, Linux and Windows"));
}

#[test]
fn labels_cannot_escape_the_definition_folder() {
    let env = Env::new();
    let out = env.run(&["daemon", "install", "--label", "../evil", "--dry-run"]);
    assert!(!out.status.success());
}

/// A free loopback port for a real service in hosted CI.
#[cfg(any(target_os = "linux", windows))]
fn free_bind() -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    format!("127.0.0.1:{}", l.local_addr().unwrap().port())
}

#[cfg(windows)]
fn wait_healthy(env: &Env, label: &str, want: bool) -> serde_json::Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let status = env.json(&["daemon", "status", "--label", label]);
        if status["healthy"] == want || std::time::Instant::now() > deadline {
            return status;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

/// The real thing on a hosted Windows runner: register, run, answer, restart, remove.
#[cfg(windows)]
#[test]
fn hosted_windows_task_scheduler_runs_the_daemon() {
    hosted_ci::require_hosted_runner();
    let env = Env::new();
    let label = test_label("hosted");
    let bind = free_bind();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // TEMPORARY DIAGNOSTIC: record Task Scheduler's view and the processes at each step.
        let diag = |step: &str| -> String {
            let q = |args: &[&str]| {
                let o = Command::new(args[0])
                    .args(&args[1..])
                    .stdin(Stdio::null())
                    .output()
                    .unwrap();
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                )
            };
            format!(
                "=== {step}\n{}\n{}\n",
                q(&["schtasks", "/Query", "/TN", &label, "/V", "/FO", "LIST"]),
                q(&[
                    "tasklist",
                    "/V",
                    "/FO",
                    "CSV",
                    "/FI",
                    "IMAGENAME eq another.exe"
                ])
            )
        };
        let mut log = String::new();
        env.json(&["daemon", "install", "--label", &label, "--bind", &bind]);
        log.push_str(&diag("right after install"));
        let status = wait_healthy(&env, &label, true);
        log.push_str(&diag("healthy after install"));
        assert_eq!(status["loaded"], true, "{status}");
        assert_eq!(status["healthy"], true, "{status}");
        env.json(&["daemon", "restart", "--label", &label]);
        log.push_str(&diag("right after restart"));
        let status = wait_healthy(&env, &label, true);
        log.push_str(&diag("healthy after restart"));
        let serve_log = std::fs::read_to_string(env.another_home().join("logs").join("serve.log"))
            .unwrap_or_default();
        panic!("DIAGNOSTIC\n{log}\n=== serve.log\n{serve_log}\n=== status {status}");
        #[allow(unreachable_code)]
        assert_eq!(status["healthy"], true, "after restart: {status}");
        assert!(env.another_home().join("logs").join("serve.log").is_file());
        // Uninstall decides "still running" from this column (review round 2): a running
        // task's last result is 267009, SCHED_S_TASK_RUNNING, in every system language.
        let query = Command::new("schtasks")
            .args(["/Query", "/TN", &label, "/V", "/FO", "CSV", "/NH"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&query.stdout);
        assert!(
            text.lines()
                .any(|l| l.contains(&format!("\"\\{label}\"")) && l.contains("\"267009\"")),
            "a running task's last result is 267009: {text}"
        );
    }));
    let removed = env.run(&["daemon", "uninstall", "--label", &label]);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    let status = wait_healthy(&env, &label, false);
    assert_eq!(status["loaded"], false, "{status}");
    assert_eq!(status["healthy"], false, "{status}");
}

/// The real thing on a hosted Linux runner, when it has a systemd user manager.
#[cfg(target_os = "linux")]
#[test]
fn hosted_linux_systemd_user_manager_runs_the_daemon() {
    hosted_ci::require_hosted_runner();
    let reachable = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stdin(Stdio::null())
        .output()
        .is_ok_and(|o| o.status.success());
    let env = Env::new();
    // Review P2-2: a binary whose path contains `$` must be the one systemd runs.
    let dollar_dir = env._dir.path().join("bin$dir");
    std::fs::create_dir_all(&dollar_dir).unwrap();
    let dollar_exe = dollar_dir.join("another");
    std::fs::copy(env!("CARGO_BIN_EXE_another"), &dollar_exe).unwrap();
    let exes = [
        ("hosted", PathBuf::from(env!("CARGO_BIN_EXE_another"))),
        ("hosted-dollar", dollar_exe),
    ];
    for (tag, exe) in &exes {
        hosted_linux_cycle(&env, reachable, tag, exe);
    }
}

#[cfg(target_os = "linux")]
fn hosted_linux_cycle(env: &Env, reachable: bool, tag: &str, exe: &Path) {
    let label = test_label(tag);
    let bind = free_bind();
    // The real manager reads the user's XDG directories, not the temporary HOME's.
    let real_home = std::env::var_os("HOME").unwrap();
    let run = |args: &[&str]| {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        env.cmd_with(exe, &all)
            .env("HOME", &real_home)
            .output()
            .unwrap()
    };
    if !reachable {
        eprintln!(
            "NOT RUN: this runner has no reachable systemd user manager; checking the refusal"
        );
        let out = run(&["daemon", "install", "--label", &label, "--bind", &bind]);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("systemd user manager"));
        run(&["daemon", "uninstall", "--label", &label]);
        return;
    }
    let status_of = || -> serde_json::Value {
        serde_json::from_slice(&run(&["daemon", "status", "--label", &label]).stdout).unwrap()
    };
    let wait = |want: bool| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let s = status_of();
            if s["healthy"] == want || std::time::Instant::now() > deadline {
                return s;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let out = run(&["daemon", "install", "--label", &label, "--bind", &bind]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        let s = wait(true);
        assert_eq!(s["loaded"], true, "{s}");
        assert!(s["pid"].as_u64().is_some_and(|p| p > 0), "{s}");
        // The running process is this binary, at this exact path.
        let pid = s["pid"].as_u64().unwrap();
        let running = std::fs::read_link(format!("/proc/{pid}/exe")).unwrap();
        assert_eq!(running, exe, "{s}");
        let out = run(&["daemon", "restart", "--label", &label]);
        assert!(out.status.success());
        let s = wait(true);
        assert_eq!(s["healthy"], true, "after restart: {s}");
    }));
    let removed = run(&["daemon", "uninstall", "--label", &label]);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    assert!(removed.status.success());
    let s = wait(false);
    assert_eq!(s["installed"], false, "{s}");
    assert_eq!(s["healthy"], false, "{s}");
}
