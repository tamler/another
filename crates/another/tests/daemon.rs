//! `another daemon`: the launchd LaunchAgent, written into a temporary HOME and never loaded.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Env {
    _dir: tempfile::TempDir,
    home: PathBuf,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        Env {
            home: dir.path().join("home"),
            _dir: dir,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_another"))
            .args(args)
            .env("HOME", &self.home)
            .env("ANOTHER_HOME", self.home.join(".another"))
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
}

#[cfg(target_os = "macos")]
#[test]
fn install_writes_a_loopback_keepalive_launch_agent() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new();
    // A label of its own, so nothing here can touch a real daemon.
    let label = format!("one.toanother.another.test-{}", std::process::id());
    let plist = env
        .home
        .join("Library/LaunchAgents")
        .join(format!("{label}.plist"));

    let out = env.run(&[
        "--json",
        "daemon",
        "install",
        "--label",
        &label,
        "--bind",
        "127.0.0.1:7499",
        "--dry-run",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let dry: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(dry["change"], "create");
    assert_eq!(dry["commands"][1][1], "bootstrap");
    assert!(!plist.exists(), "dry run writes nothing");

    let out = env.run(&[
        "--json",
        "daemon",
        "install",
        "--label",
        &label,
        "--bind",
        "127.0.0.1:7499",
        "--no-load",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
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

    let again: serde_json::Value = serde_json::from_slice(
        &env.run(&[
            "--json",
            "daemon",
            "install",
            "--label",
            &label,
            "--bind",
            "127.0.0.1:7499",
            "--no-load",
        ])
        .stdout,
    )
    .unwrap();
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
    let out = env.run(&[
        "--json",
        "daemon",
        "uninstall",
        "--label",
        &label,
        "--dry-run",
    ]);
    assert!(out.status.success());
    let dry: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
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

#[cfg(not(target_os = "macos"))]
#[test]
fn install_explains_that_launchd_is_macos_only() {
    let env = Env::new();
    let out = env.run(&["daemon", "install", "--no-load"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("macOS only"));
}

#[test]
fn labels_cannot_escape_the_launch_agents_folder() {
    let env = Env::new();
    let out = env.run(&["daemon", "install", "--label", "../evil", "--dry-run"]);
    assert!(!out.status.success());
}
