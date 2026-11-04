//! `another service`: full install / update / status / stop / start / uninstall cycles for
//! systemd and launchd, driven against fake `systemctl`, `journalctl`, `launchctl` and `id`
//! binaries that record what they are asked to do. Nothing touches a real service manager,
//! and every run uses a temporary HOME / ANOTHER_HOME and a free, non-default port.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

struct Env {
    _dir: tempfile::TempDir,
    user_home: PathBuf,
    data: PathBuf,
    bin: PathBuf,
    log: PathBuf,
    platform: &'static str,
}

const FAKES: &[(&str, &str)] = &[
    (
        "systemctl",
        r#"[ -n "$NO_BUS" ] && { echo "Failed to connect to bus: No medium found" >&2; exit 1; }
case "$*" in
  *"enable --now"*) [ -n "$FAIL_ENABLE" ] && { echo "Unit another.service has a bad setting" >&2; exit 1; }; echo active > "$STATE" ;;
  *restart*) [ -n "$FAIL_RESTART" ] && { echo "Job for another.service failed" >&2; exit 1; }; echo active > "$STATE" ;;
  *" start "*) echo active > "$STATE" ;;
  *"disable --now"*|*" stop "*) echo inactive > "$STATE" ;;
  *is-active*) s=$(cat "$STATE" 2>/dev/null || echo inactive); echo "$s"; [ "$s" = active ] || exit 3 ;;
  *MainPID*) if [ "$(cat "$STATE" 2>/dev/null)" = active ]; then echo 4242; else echo 0; fi ;;
esac
exit 0"#,
    ),
    (
        "journalctl",
        "echo 'another line one'\necho 'another line two'",
    ),
    (
        "launchctl",
        r#"case "$1" in
  print) [ -f "$STATE" ] || exit 113; echo "state = running"; echo "	pid = 4343" ;;
  bootstrap)
    if [ -n "$FAIL_BOOTSTRAP_ONCE" ] && [ ! -f "$FAIL_BOOTSTRAP_ONCE" ]; then
      touch "$FAIL_BOOTSTRAP_ONCE"; echo "Bootstrap failed: 1: Operation not permitted" >&2; exit 1
    fi
    touch "$STATE" ;;
  bootout)
    [ -f "$STATE" ] || exit 3
    if [ -n "$SLOW_BOOTOUT_ONCE" ] && [ ! -f "$SLOW_BOOTOUT_ONCE" ]; then
      touch "$SLOW_BOOTOUT_ONCE"; exit 0
    fi
    rm -f "$STATE" ;;
esac
exit 0"#,
    ),
    ("id", "echo 501"),
];

impl Env {
    fn new(platform: &'static str) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fake bin");
        std::fs::create_dir_all(&bin).unwrap();
        let log = dir.path().join("calls.log");
        for (name, body) in FAKES {
            let script = bin.join(name);
            std::fs::write(
                &script,
                format!("#!/bin/sh\necho \"{name} $*\" >> \"$FAKE_LOG\"\n{body}\n"),
            )
            .unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Env {
            user_home: dir.path().join("user home"),
            data: dir.path().join("Another Data"),
            _dir: dir,
            bin,
            log,
            platform,
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_another"));
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        c.args(args)
            .env("HOME", &self.user_home)
            .env("ANOTHER_HOME", &self.data)
            .env("ANOTHER_SERVICE_PLATFORM", self.platform)
            .env("PATH", path)
            .env("FAKE_LOG", &self.log)
            .env("STATE", self.user_home.join(".fake-state"))
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_SERVER")
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let out = self.run(&all);
        assert!(
            out.status.success(),
            "another {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn clear_calls(&self) {
        let _ = std::fs::remove_file(&self.log);
    }
}

/// A port below the ephemeral range, unique within this test run and picked without opening a
/// socket. A probe listener could leak into a child that a parallel test is spawning: macOS has
/// no atomic close-on-exec for sockets, and `install`'s port check would then see that child
/// listening on this test's port.
fn free_port() -> u16 {
    use std::sync::atomic::{AtomicU16, Ordering};
    static NEXT: AtomicU16 = AtomicU16::new(0);
    let offset = NEXT.fetch_add(1, Ordering::Relaxed);
    let port = 20_000 + (std::process::id() % 10_000) as u16 + offset;
    assert_ne!(port, 7466);
    port
}

fn unit_path(env: &Env) -> PathBuf {
    env.user_home.join(".config/systemd/user/another.service")
}

fn plist_path(env: &Env) -> PathBuf {
    env.user_home
        .join("Library/LaunchAgents/one.toanother.another.plist")
}

#[test]
fn nothing_is_installed_by_default() {
    let env = Env::new("systemd");
    env.json(&["init"]);
    env.json(&["agent", "register", "a", "--program", "other"]);
    let status = env.json(&["service", "status"]);
    assert_eq!(status["installed"], false);
    assert!(!unit_path(&env).exists());
    assert!(env.calls().iter().all(|c| !c.contains("enable")));
}

#[test]
fn systemd_install_update_status_uninstall() {
    let env = Env::new("systemd");
    let bind = format!("127.0.0.1:{}", free_port());
    let r = env.json(&[
        "service",
        "install",
        "--bind",
        &bind,
        "--auth",
        "--allowed-host",
        "box.tail-net.ts.net",
    ]);
    assert_eq!(r["action"], "installed");
    assert!(r["hints"]
        .as_array()
        .unwrap()
        .iter()
        .any(|h| h.as_str().unwrap().contains("enable-linger")));
    let unit = std::fs::read_to_string(unit_path(&env)).unwrap();
    assert!(unit.contains("Restart=on-failure"));
    assert!(unit.contains(&format!("\"--bind\" \"{bind}\"")), "{unit}");
    assert!(unit.contains("\"--auth\" \"--allowed-host\" \"box.tail-net.ts.net\""));
    assert!(unit.contains(&format!("\"--home\" \"{}\"", env.data.display())));
    assert!(env.data.join("another.db").exists(), "home initialised");
    let calls = env.calls();
    assert!(
        calls.contains(&"systemctl --user daemon-reload".to_string()),
        "{calls:?}"
    );
    assert!(calls.contains(&"systemctl --user enable --now another.service".to_string()));

    let st = env.json(&["service", "status"]);
    assert_eq!(st["installed"], true);
    assert_eq!(st["loaded"], true);
    assert_eq!(st["pid"], 4242);
    assert_eq!(st["bind"], bind.as_str());
    assert_eq!(st["log_lines"][1], "another line two");

    // Installing again updates in place and restarts.
    env.clear_calls();
    let again = env.json(&["service", "install", "--bind", &bind]);
    assert_eq!(again["action"], "updated");
    assert!(!std::fs::read_to_string(unit_path(&env))
        .unwrap()
        .contains("--auth"));
    assert!(env
        .calls()
        .contains(&"systemctl --user restart another.service".to_string()));

    env.json(&["service", "stop"]);
    assert_eq!(env.json(&["service", "status"])["loaded"], false);
    env.json(&["service", "start"]);
    assert_eq!(env.json(&["service", "status"])["loaded"], true);
    let logs = env.json(&["service", "logs", "-n", "5"]);
    assert_eq!(logs[0], "another line one");

    env.clear_calls();
    let un = env.json(&["service", "uninstall"]);
    assert_eq!(un["removed"], true);
    assert!(!unit_path(&env).exists());
    assert!(env
        .calls()
        .contains(&"systemctl --user disable --now another.service".to_string()));
    assert_eq!(
        env.json(&["service", "uninstall"])["removed"],
        false,
        "idempotent"
    );
    assert_eq!(env.json(&["service", "status"])["installed"], false);
    let start = env.run(&["service", "start"]);
    assert!(!start.status.success());
}

#[test]
fn launchd_install_update_status_uninstall() {
    let env = Env::new("launchd");
    let bind = format!("127.0.0.1:{}", free_port());
    let r = env.json(&["service", "install", "--bind", &bind]);
    assert_eq!(r["action"], "installed");
    let path = plist_path(&env);
    let plist = std::fs::read_to_string(&path).unwrap();
    assert!(plist.contains("<string>one.toanother.another</string>"));
    assert!(plist.contains(&format!("<string>{bind}</string>")));
    assert!(plist.contains(&format!(
        "<string>{}</string>",
        env.data.join("logs").join("another.err.log").display()
    )));
    assert!(env.data.join("logs").is_dir(), "log dir created");
    let calls = env.calls();
    assert!(
        calls.contains(&format!("launchctl bootstrap gui/501 {}", path.display())),
        "{calls:?}"
    );

    std::fs::write(
        env.data.join("logs/another.err.log"),
        "started\nlistening\n",
    )
    .unwrap();
    let st = env.json(&["service", "status", "--lines", "1"]);
    assert_eq!(st["loaded"], true);
    assert_eq!(st["pid"], 4343);
    assert_eq!(st["bind"], bind.as_str());
    assert_eq!(st["log_lines"], serde_json::json!(["listening"]));

    // Re-install reloads the agent with the new definition.
    env.clear_calls();
    assert_eq!(
        env.json(&["service", "install", "--bind", &bind, "--federation"])["action"],
        "updated"
    );
    let calls = env.calls();
    assert!(calls.contains(&"launchctl bootout gui/501/one.toanother.another".to_string()));
    assert!(calls
        .iter()
        .any(|c| c.starts_with("launchctl bootstrap gui/501 ")));
    assert!(std::fs::read_to_string(&path)
        .unwrap()
        .contains("<string>--federation</string>"));

    env.json(&["service", "stop"]);
    assert_eq!(env.json(&["service", "status"])["loaded"], false);
    assert!(path.exists(), "stop keeps it installed");
    env.json(&["service", "start"]);
    assert_eq!(env.json(&["service", "status"])["loaded"], true);

    assert_eq!(env.json(&["service", "uninstall"])["removed"], true);
    assert!(!path.exists());
    assert_eq!(env.json(&["service", "status"])["installed"], false);
}

#[test]
fn refuses_a_port_already_in_use_and_unsafe_binds() {
    let env = Env::new("systemd");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let bind = listener.local_addr().unwrap().to_string();
    let out = env.run(&["service", "install", "--bind", &bind]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("already listening on"), "{err}");
    assert!(!unit_path(&env).exists(), "nothing written");
    drop(listener);

    let out = env.run(&["service", "install", "--bind", "0.0.0.0:7499"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("refusing to bind"));
    assert!(!unit_path(&env).exists());
}

#[test]
fn paths_with_spaces_survive_both_formats() {
    // HOME, ANOTHER_HOME and the fake bin directory all contain spaces in this fixture.
    for platform in ["systemd", "launchd"] {
        let env = Env::new(platform);
        let bind = format!("127.0.0.1:{}", free_port());
        env.json(&["service", "install", "--bind", &bind]);
        let st = env.json(&["service", "status"]);
        let args: Vec<String> = st["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap().to_string())
            .collect();
        let home_at = args.iter().position(|a| a == "--home").unwrap();
        assert_eq!(Path::new(&args[home_at + 1]), env.data, "{platform}");
        env.json(&["service", "uninstall"]);
    }
}

#[test]
fn generated_unit_passes_systemd_analyze_verify() {
    let Ok(probe) = Command::new("systemd-analyze").arg("--version").output() else {
        eprintln!("systemd-analyze not available; skipping");
        return;
    };
    if !probe.status.success() {
        return;
    }
    let env = Env::new("systemd");
    let bind = format!("127.0.0.1:{}", free_port());
    env.json(&[
        "service",
        "install",
        "--bind",
        &bind,
        "--auth",
        "--allowed-host",
        "box.ts.net",
    ]);
    let out = Command::new("systemd-analyze")
        // System mode checks the same unit syntax without needing a user manager.
        .arg("verify")
        .arg(unit_path(&env))
        .env("HOME", &env.user_home)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !text.contains("Failed to initialize manager"),
        "verify did not run: {text}"
    );
    assert!(!text.contains("another.service:"), "{text}");
    assert!(!text.contains("bad unit file setting"), "{text}");
    assert!(!text.contains("fatal error"), "{text}");
}

#[test]
fn wildcard_listener_on_the_port_is_a_conflict() {
    let env = Env::new("systemd");
    let listener = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let out = env.run(&["service", "install", "--bind", &format!("127.0.0.1:{port}")]);
    if String::from_utf8_lossy(&out.stderr).contains("already listening") {
        assert!(!out.status.success());
        assert!(!unit_path(&env).exists());
    } else {
        // Linux refuses the test bind anyway; without lsof/ss the scanner can't help.
        assert!(!out.status.success() || crate_scanner_missing());
    }
    drop(listener);
}

fn crate_scanner_missing() -> bool {
    Command::new("lsof").arg("-v").output().is_err()
        && Command::new("ss").arg("-V").output().is_err()
}

#[test]
fn failed_activation_leaves_nothing_installed() {
    let env = Env::new("systemd");
    let bind = format!("127.0.0.1:{}", free_port());
    let out = env
        .cmd(&["service", "install", "--bind", &bind])
        .env("FAIL_ENABLE", "1")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("nothing was left installed"), "{err}");
    assert!(!unit_path(&env).exists(), "new unit rolled back");

    // An update that fails restores the previous definition.
    env.json(&["service", "install", "--bind", &bind]);
    let before = std::fs::read_to_string(unit_path(&env)).unwrap();
    let out = env
        .cmd(&["service", "install", "--bind", &bind, "--federation"])
        .env("FAIL_ENABLE", "1")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(std::fs::read_to_string(unit_path(&env)).unwrap(), before);
}

#[test]
fn failed_new_unit_does_not_leave_an_enable_link() {
    let env = Env::new("systemd");
    let bind = format!("127.0.0.1:{}", free_port());
    let out = env
        .cmd(&["service", "install", "--bind", &bind])
        .env("FAIL_ENABLE", "1")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let calls = env.calls();
    let disable = calls
        .iter()
        .position(|c| c == "systemctl --user disable another.service");
    let reload = calls
        .iter()
        .rposition(|c| c == "systemctl --user daemon-reload");
    assert!(disable.is_some(), "{calls:?}");
    assert!(disable < reload, "{calls:?}");
}

#[test]
fn failed_systemd_update_restarts_the_previous_service() {
    let env = Env::new("systemd");
    let bind = format!("127.0.0.1:{}", free_port());
    env.json(&["service", "install", "--bind", &bind]);
    env.clear_calls();
    let out = env
        .cmd(&["service", "install", "--bind", &bind, "--federation"])
        .env("FAIL_ENABLE", "1")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("nothing was left installed"), "{err}");
    assert!(err.contains("previous service was restored"), "{err}");
    let calls = env.calls();
    assert_eq!(
        calls.last().map(String::as_str),
        Some("systemctl --user restart another.service"),
        "{calls:?}"
    );
    let failed = calls.iter().rposition(|c| c.contains("enable --now"));
    let reload = calls
        .iter()
        .rposition(|c| c == "systemctl --user daemon-reload");
    assert!(reload > failed, "restored unit is reloaded: {calls:?}");
}

#[test]
fn failed_systemd_update_of_a_stopped_service_leaves_it_stopped() {
    let env = Env::new("systemd");
    let bind = format!("127.0.0.1:{}", free_port());
    env.json(&["service", "install", "--bind", &bind]);
    env.json(&["service", "stop"]);
    env.clear_calls();
    let out = env
        .cmd(&["service", "install", "--bind", &bind, "--federation"])
        .env("FAIL_RESTART", "1")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("previous definition was restored"), "{err}");
    assert_eq!(
        env.calls().last().map(String::as_str),
        Some("systemctl --user stop another.service"),
        "`enable --now` had started the new definition"
    );
    assert_eq!(env.json(&["service", "status"])["loaded"], false);
}

#[test]
fn slow_launchd_unload_during_an_update_still_reloads_the_previous_service() {
    let env = Env::new("launchd");
    let bind = format!("127.0.0.1:{}", free_port());
    env.json(&["service", "install", "--bind", &bind]);
    env.clear_calls();
    // launchd accepts the unload but the job outlasts the wait, then goes away: still being
    // loaded at rollback doesn't mean it survived.
    let out = env
        .cmd(&["service", "install", "--bind", &bind, "--federation"])
        .env("SLOW_BOOTOUT_ONCE", env.user_home.join(".slow-once"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("still unloading"), "{err}");
    let calls = env.calls();
    assert!(
        calls.iter().any(|c| c.starts_with("launchctl bootstrap ")),
        "the previous definition is loaded again: {calls:?}"
    );
}

#[test]
fn failed_launchd_update_reloads_the_previous_service() {
    let env = Env::new("launchd");
    let bind = format!("127.0.0.1:{}", free_port());
    env.json(&["service", "install", "--bind", &bind]);
    let before = std::fs::read_to_string(plist_path(&env)).unwrap();
    env.clear_calls();
    let out = env
        .cmd(&["service", "install", "--bind", &bind, "--federation"])
        .env("FAIL_BOOTSTRAP_ONCE", env.user_home.join(".failed-once"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("nothing was left installed"), "{err}");
    assert!(err.contains("previous service was restored"), "{err}");
    assert_eq!(std::fs::read_to_string(plist_path(&env)).unwrap(), before);
    let bootstraps = env
        .calls()
        .iter()
        .filter(|c| c.starts_with("launchctl bootstrap "))
        .count();
    assert_eq!(bootstraps, 2, "the old definition is loaded again");
    assert_eq!(env.json(&["service", "status"])["loaded"], true);
}

#[test]
fn update_warns_about_what_it_drops() {
    let env = Env::new("systemd");
    let bind = format!("127.0.0.1:{}", free_port());
    env.json(&[
        "service",
        "install",
        "--bind",
        &bind,
        "--auth",
        "--allowed-host",
        "box.example.ts.net",
    ]);

    let same = env.json(&[
        "service",
        "install",
        "--bind",
        &bind,
        "--auth",
        "--allowed-host",
        "box.example.ts.net",
    ]);
    let warnings = |r: &Value| r["warnings"].as_array().unwrap().clone();
    assert!(warnings(&same).is_empty(), "{same}");

    let other_home = env.user_home.join("other data");
    let other_home = other_home.to_str().unwrap();
    let changed = env.json(&["--home", other_home, "service", "install", "--bind", &bind]);
    let text = warnings(&changed)
        .iter()
        .map(|w| w.as_str().unwrap().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("no longer requires bearer tokens"), "{text}");
    assert!(text.contains("box.example.ts.net"), "{text}");
    assert!(text.contains(other_home), "{text}");

    let human = env.run(&["service", "install", "--bind", &bind, "--auth"]);
    assert!(
        String::from_utf8_lossy(&human.stdout).contains("warning: "),
        "the data directory moved back, and text output shows it"
    );
}

#[test]
fn trusted_proxy_header_reaches_the_installed_serve_command() {
    for platform in ["systemd", "launchd"] {
        let env = Env::new(platform);
        let port = free_port();
        let bind = format!("127.0.0.1:{port}");
        let refused = env.run(&[
            "service",
            "install",
            "--bind",
            &bind,
            "--trusted-proxy-header",
            "x-real-ip",
        ]);
        assert!(!refused.status.success(), "the flag needs --auth");
        assert!(
            !unit_path(&env).exists() && !plist_path(&env).exists(),
            "a refused install writes nothing"
        );

        env.json(&[
            "service",
            "install",
            "--bind",
            &bind,
            "--auth",
            "--trusted-proxy-header",
            "x-real-ip",
        ]);
        let status = env.json(&["service", "status", "--lines", "0"]);
        let args: Vec<String> = status["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap().to_string())
            .collect();
        let at = args
            .iter()
            .position(|a| a == "--trusted-proxy-header")
            .unwrap_or_else(|| panic!("{platform}: flag missing from {args:?}"));
        assert_eq!(args[at + 1], "x-real-ip", "{platform}");

        // The written command line is one `serve` accepts: the fake manager doesn't run it, so
        // run it here.
        let mut server = Server(
            Command::new(&args[0])
                .args(&args[1..])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            if let Some(exit) = server.0.try_wait().unwrap() {
                panic!("{platform}: serve refused the installed flags: {exit}");
            }
            assert!(std::time::Instant::now() < deadline, "serve never listened");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        drop(server);

        let same = env.json(&[
            "service",
            "install",
            "--bind",
            &bind,
            "--auth",
            "--trusted-proxy-header",
            "x-real-ip",
        ]);
        assert_eq!(same["warnings"], serde_json::json!([]), "{same}");
        let changed = env.json(&[
            "service",
            "install",
            "--bind",
            &bind,
            "--auth",
            "--trusted-proxy-header",
            "cf-connecting-ip",
        ]);
        assert!(changed["warnings"][0]
            .as_str()
            .unwrap()
            .contains("x-real-ip"));
        let dropped = env.json(&["service", "install", "--bind", &bind, "--auth"]);
        let text = dropped["warnings"][0].as_str().unwrap();
        assert!(
            text.contains("--trusted-proxy-header cf-connecting-ip was dropped"),
            "{text}"
        );
    }
}

/// `another doctor --json [args]`; it exits 1 on a failed check, so the status isn't asserted.
fn doctor(env: &Env, args: &[&str]) -> Value {
    let mut all = vec!["--json", "doctor"];
    all.extend_from_slice(args);
    let out = env.run(&all);
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)))
}

fn doctor_check(report: &Value, area: &str, level: &str) -> (String, String) {
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["area"] == area && c["level"] == level)
        .unwrap_or_else(|| panic!("no {level} {area} in {report}"));
    (
        check["detail"].as_str().unwrap().to_string(),
        check["fix"].as_str().unwrap_or_default().to_string(),
    )
}

/// A real `another serve`, stopped on drop so a failed assertion doesn't leave it running.
struct Server(std::process::Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn serve_on(env: &Env, port: u16) -> Server {
    let bind = format!("127.0.0.1:{port}");
    let server = Server(
        env.cmd(&["serve", "--bind", &bind])
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(std::time::Instant::now() < deadline, "serve never listened");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    server
}

#[test]
fn doctor_reaches_a_wildcard_bind_through_loopback() {
    use another::doctor::bind_url;
    assert_eq!(bind_url("127.0.0.1:7467"), "http://127.0.0.1:7467");
    assert_eq!(bind_url("0.0.0.0:7467"), "http://127.0.0.1:7467");
    assert_eq!(bind_url("[::]:7467"), "http://[::1]:7467");
    assert_eq!(bind_url("[::1]:7467"), "http://[::1]:7467");
    assert_eq!(bind_url("192.168.1.5:7467"), "http://192.168.1.5:7467");
    assert_eq!(bind_url("localhost:7467"), "http://localhost:7467");
}

#[test]
fn doctor_checks_the_installed_service_instead_of_the_default_port() {
    for platform in ["systemd", "launchd"] {
        let env = Env::new(platform);
        let port = free_port();
        let bind = format!("127.0.0.1:{port}");
        env.json(&["service", "install", "--bind", &bind]);
        let _server = serve_on(&env, port);

        let report = doctor(&env, &[]);
        let (detail, _) = doctor_check(&report, "server", "pass");
        assert!(detail.contains(&bind), "{platform}: {detail}");
        let (detail, _) = doctor_check(&report, "service", "pass");
        assert!(detail.contains(&bind), "{platform}: {detail}");

        // The same directory through a symlink is the same home.
        let link = env.user_home.join("home link");
        std::os::unix::fs::symlink(&env.data, &link).unwrap();
        let link = link.to_str().unwrap();
        let report = doctor(&env, &["--server", &format!("http://{bind}")]);
        doctor_check(&report, "service", "pass");
        let out = env.run(&["--json", "--home", link, "doctor"]);
        let report: Value = serde_json::from_slice(&out.stdout).unwrap();
        let (detail, _) = doctor_check(&report, "server", "pass");
        assert!(detail.contains(&bind), "{platform}: {detail}");
        doctor_check(&report, "service", "pass");
    }
}

#[test]
fn doctor_fix_hints_name_the_service_when_it_is_down() {
    for platform in ["systemd", "launchd"] {
        let env = Env::new(platform);
        let bind = format!("127.0.0.1:{}", free_port());
        env.json(&["service", "install", "--bind", &bind]);

        // Loaded, but nothing answers: the logs say why, and starting it again wouldn't help.
        let report = doctor(&env, &[]);
        let (detail, fix) = doctor_check(&report, "server", "fail");
        assert!(detail.contains(&bind), "{platform}: {detail}");
        assert!(fix.contains("another service logs"), "{platform}: {fix}");

        env.json(&["service", "stop"]);
        let report = doctor(&env, &[]);
        let (_, fix) = doctor_check(&report, "server", "fail");
        assert!(fix.contains("another service start"), "{platform}: {fix}");
        assert!(!fix.contains("another serve"), "{platform}: {fix}");
        let (_, fix) = doctor_check(&report, "service", "warn");
        assert!(fix.contains("another service start"), "{platform}: {fix}");
    }
}

#[test]
fn doctor_fails_a_service_whose_binary_is_gone() {
    let env = Env::new("launchd");
    let bind = format!("127.0.0.1:{}", free_port());
    env.json(&["service", "install", "--bind", &bind]);
    let path = plist_path(&env);
    let text = std::fs::read_to_string(&path).unwrap();
    let real = env!("CARGO_BIN_EXE_another");
    assert!(text.contains(real), "{text}");
    std::fs::write(&path, text.replace(real, "/nonexistent/another")).unwrap();

    let report = doctor(&env, &[]);
    let (detail, fix) = doctor_check(&report, "service", "fail");
    assert!(detail.contains("/nonexistent/another"), "{detail}");
    assert!(fix.contains("another service install"), "{fix}");
    // It is still this home's server, so doctor checks its address, not the default port.
    let (detail, fix) = doctor_check(&report, "server", "fail");
    assert!(detail.contains(&bind), "{detail}");
    assert!(fix.contains("another service install"), "{fix}");
}

#[test]
fn doctor_says_the_user_manager_is_unreachable_instead_of_stopped() {
    let env = Env::new("systemd");
    let bind = format!("127.0.0.1:{}", free_port());
    env.json(&["service", "install", "--bind", &bind]);
    let out = env
        .cmd(&["--json", "doctor"])
        .env("NO_BUS", "1")
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let (detail, fix) = doctor_check(&report, "service", "warn");
    assert!(detail.contains("Failed to connect to bus"), "{detail}");
    assert!(!fix.contains("another service start"), "{fix}");
    let (detail, fix) = doctor_check(&report, "server", "fail");
    assert!(detail.contains(&bind), "{detail}");
    assert!(!fix.contains("another service start"), "{fix}");
}

#[test]
fn doctor_tells_a_service_user_to_move_the_service() {
    let env = Env::new("launchd");
    let port = free_port();
    let bind = format!("127.0.0.1:{port}");
    env.json(&["service", "install", "--bind", &bind]);
    env.json(&["agent", "register", "claude", "--program", "claude"]);
    env.json(&[
        "resource",
        "register",
        "dev_server",
        "web",
        "--agent",
        "claude",
        "--port",
        &port.to_string(),
    ]);

    // Something else answers on the service's port.
    let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        while let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.read(&mut [0; 1024]);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi");
        }
    });

    let report = doctor(&env, &[]);
    let (_, fix) = doctor_check(&report, "server", "fail");
    assert!(fix.contains("another service install --bind"), "{fix}");
    let (_, fix) = doctor_check(&report, "ports", "fail");
    assert!(fix.contains("another service install --bind"), "{fix}");
}

#[test]
fn doctor_warns_about_a_definition_it_cannot_read() {
    let env = Env::new("systemd");
    let bind = format!("127.0.0.1:{}", free_port());
    env.json(&["service", "install", "--bind", &bind]);
    std::fs::write(unit_path(&env), "[Service]\n").unwrap();
    let dead = format!("http://127.0.0.1:{}", free_port());
    let report = doctor(&env, &["--server", &dead]);
    let (_, fix) = doctor_check(&report, "service", "warn");
    assert!(fix.contains("another service install"), "{fix}");
}

#[test]
fn doctor_keeps_the_default_server_when_the_service_uses_another_home() {
    let env = Env::new("systemd");
    let port = free_port();
    let bind = format!("127.0.0.1:{port}");
    let other = env.user_home.join("other data");
    let other = other.to_str().unwrap();
    env.json(&["--home", other, "service", "install", "--bind", &bind]);
    std::fs::create_dir_all(&env.data).unwrap();
    env.json(&["init"]);

    let report = doctor(&env, &[]);
    let (detail, fix) = doctor_check(&report, "service", "warn");
    assert!(detail.contains(other), "{detail}");
    assert!(fix.contains("--home"), "{fix}");
    let server = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["area"] == "server")
        .unwrap();
    let detail = server["detail"].as_str().unwrap();
    assert!(!detail.contains(&bind), "{detail}");
    assert!(detail.contains(":7466"), "{detail}");
}

#[test]
fn doctor_prefers_an_explicit_server_over_the_service() {
    let env = Env::new("launchd");
    let bind = format!("127.0.0.1:{}", free_port());
    env.json(&["service", "install", "--bind", &bind]);
    let dead_port = free_port();
    let dead = format!("http://127.0.0.1:{dead_port}");

    let report = doctor(&env, &["--server", &dead]);
    let (detail, fix) = doctor_check(&report, "server", "fail");
    assert!(detail.contains(&dead), "{detail}");
    assert!(
        fix.contains("another serve"),
        "this address isn't the service: {fix}"
    );

    let out = env
        .cmd(&["--json", "doctor"])
        .env("ANOTHER_SERVER", &dead)
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let (detail, _) = doctor_check(&report, "server", "fail");
    assert!(
        detail.contains(&dead),
        "ANOTHER_SERVER counts too: {detail}"
    );
}

#[test]
fn doctor_does_not_ask_a_manager_when_no_service_is_installed() {
    for platform in ["systemd", "launchd"] {
        let env = Env::new(platform);
        let dead = format!("http://127.0.0.1:{}", free_port());
        let report = doctor(&env, &["--server", &dead]);
        doctor_check(&report, "service", "skip");
        assert!(env.calls().is_empty(), "{platform}: {:?}", env.calls());
    }
}

#[test]
fn doctor_warns_when_the_manager_cannot_be_asked() {
    let env = Env::new("systemd");
    let bind = format!("127.0.0.1:{}", free_port());
    env.json(&["service", "install", "--bind", &bind]);
    std::fs::remove_file(env.bin.join("systemctl")).unwrap();
    // PATH holds only the remaining fakes, so a real systemctl can't answer either.
    let out = env
        .cmd(&["--json", "doctor"])
        .env("PATH", &env.bin)
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let (detail, _) = doctor_check(&report, "service", "warn");
    assert!(detail.contains("systemctl"), "{detail}");
    // The definition still says where the server should be.
    let (detail, _) = doctor_check(&report, "server", "fail");
    assert!(detail.contains(&bind), "{detail}");
}

#[test]
fn serve_shuts_down_gracefully_on_sigterm() {
    let env = Env::new("launchd");
    let port = free_port();
    let bind = format!("127.0.0.1:{port}");
    let mut child = env
        .cmd(&["serve", "--bind", &bind])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(std::time::Instant::now() < deadline, "serve never listened");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let status = terminate(&mut child).0;
    assert_eq!(status.code(), Some(0), "a clean exit, not death by signal");
}

#[test]
fn sigterm_is_not_held_up_by_a_stalled_request() {
    use std::io::Write;
    let env = Env::new("launchd");
    let port = free_port();
    let bind = format!("127.0.0.1:{port}");
    let mut child = env
        .cmd(&["serve", "--bind", &bind])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut stalled = loop {
        if let Ok(s) = std::net::TcpStream::connect(("127.0.0.1", port)) {
            break s;
        }
        assert!(std::time::Instant::now() < deadline, "serve never listened");
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    // A request whose body never finishes keeps its connection in flight.
    write!(
        stalled,
        "POST /mcp HTTP/1.1\r\nHost: {bind}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: 1000\r\n\r\n{{"
    )
    .unwrap();
    stalled.flush().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let (status, took) = terminate(&mut child);
    assert_eq!(status.code(), Some(0));
    assert!(
        took >= std::time::Duration::from_secs(2),
        "the request should have held shutdown until the grace period: {took:?}"
    );
    drop(stalled);
}

/// SIGTERM a process and wait for it, up to 10 s. Returns its status and how long it took.
fn terminate(child: &mut std::process::Child) -> (std::process::ExitStatus, std::time::Duration) {
    let killed = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return (status, start.elapsed());
        }
        if start.elapsed() >= std::time::Duration::from_secs(10) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("serve did not exit after SIGTERM");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[test]
fn no_user_bus_is_refused_before_writing() {
    let env = Env::new("systemd");
    let bind = format!("127.0.0.1:{}", free_port());
    let out = env
        .cmd(&["service", "install", "--bind", &bind])
        .env("NO_BUS", "1")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("enable-linger"));
    assert!(!unit_path(&env).exists());
}
