//! Doctor reads a systemd user unit without changing it or contacting a real manager.
#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};

#[test]
fn doctor_checks_the_systemd_user_unit() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let data = dir.path().join("data");
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let manager = bin.join("systemctl");
    std::fs::write(
        &manager,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$HOME/calls\"\nprintf 'LoadState=loaded\\nUnitFileState=%s\\nActiveState=inactive\\nMainPID=0\\n' \"$TEST_UNIT_STATE\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&manager, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(paths).unwrap();
    let run = |args: &[&str], state: &str| -> Output {
        Command::new(env!("CARGO_BIN_EXE_another"))
            .args(args)
            .env("HOME", &home)
            .env("ANOTHER_HOME", &data)
            .env("PATH", &path)
            .env("TEST_UNIT_STATE", state)
            .env_remove("XDG_CONFIG_HOME")
            .output()
            .unwrap()
    };
    assert!(run(&["init"], "enabled").status.success());
    assert!(run(&["daemon", "install", "--no-load"], "enabled")
        .status
        .success());
    let unit = home.join(".config/systemd/user/one.toanother.another.service");
    let before = std::fs::read(&unit).unwrap();
    // The server check is deliberately aimed at a closed scratch loopback port.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let bind = listener.local_addr().unwrap().to_string();
    drop(listener);
    for state in ["enabled", "disabled"] {
        let out = run(&["--json", "doctor", "--bind", &bind], state);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let daemon = report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == "daemon")
            .unwrap();
        assert_eq!(daemon["status"], "warn", "{daemon}");
        let detail = daemon["detail"].as_str().unwrap();
        if state == "enabled" {
            assert!(
                detail.contains("loaded") && detail.contains("did not answer"),
                "{detail}"
            );
        } else {
            assert!(detail.contains("installed but not loaded"), "{detail}");
        }
        assert_eq!(std::fs::read(&unit).unwrap(), before);
    }
    let calls = std::fs::read_to_string(home.join("calls")).unwrap();
    assert_eq!(calls.lines().count(), 2, "{calls}");
    assert!(
        calls.lines().all(|line| line.starts_with("--user show ")),
        "{calls}"
    );
}
