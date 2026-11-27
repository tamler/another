//! Operator warnings at HTTP startup and daemon installation, using isolated homes.

use std::process::Command;

#[path = "common/cli.rs"]
mod cli;

use cli::another_cmd;

fn isolated(dir: &std::path::Path) -> Command {
    let mut cmd = another_cmd(&dir.join(".another"));
    cmd.env("HOME", dir).env_remove("XDG_CONFIG_HOME");
    cmd
}

fn assert_exposure_warning(stderr: &str) {
    for text in [
        "warning:",
        "do not tunnel or reverse-proxy this port",
        "checks no token",
        "rewrites Host to loopback",
        "remote callers look local",
        "read the feed and act as any agent",
    ] {
        assert!(stderr.contains(text), "missing {text:?}: {stderr}");
    }
}

#[test]
fn daemon_install_warns_without_polluting_json() {
    let dir = tempfile::tempdir().unwrap();
    let out = isolated(dir.path())
        .args(["--json", "daemon", "install", "--no-load", "--dry-run"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap();
    assert_exposure_warning(&String::from_utf8_lossy(&out.stderr));
}

#[cfg(unix)]
fn startup_stderr(flags: &[&str]) -> String {
    use std::process::{Child, Stdio};
    use std::time::{Duration, Instant};

    struct Server(Child);
    impl Drop for Server {
        fn drop(&mut self) {
            if self.0.try_wait().unwrap().is_none() {
                assert!(Command::new("kill")
                    .args(["-TERM", &self.0.id().to_string()])
                    .status()
                    .unwrap()
                    .success());
                let started = Instant::now();
                while self.0.try_wait().unwrap().is_none() {
                    assert!(
                        started.elapsed() < Duration::from_secs(15),
                        "SIGTERM ignored"
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("stderr.log");
    let mut server = Server(
        isolated(dir.path())
            .args(["serve", "--bind", "127.0.0.1:0", "--no-wake"])
            .args(flags)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap(),
    );
    let started = Instant::now();
    loop {
        let text = std::fs::read_to_string(&log).unwrap();
        if text.contains("HTTP on http://") || text.contains("HTTP) on http://") {
            return text;
        }
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "server exited: {text}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "startup timed out: {text}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
#[test]
fn plain_loopback_startup_warns_even_without_dashboard() {
    assert_exposure_warning(&startup_stderr(&["--no-dashboard"]));
}

#[cfg(unix)]
#[test]
fn federation_only_startup_does_not_claim_local_agent_exposure() {
    let text = startup_stderr(&["--federation"]);
    assert!(text.contains("federation-only HTTP"), "{text}");
    assert!(!text.contains("act as any agent"), "{text}");
    assert!(!text.contains("read the feed"), "{text}");
}

#[cfg(unix)]
#[test]
fn authenticated_startup_does_not_claim_no_token_check() {
    let text = startup_stderr(&["--auth"]);
    assert!(text.contains("bearer token required"), "{text}");
    assert!(!text.contains("checks no token"), "{text}");
    assert!(!text.contains("act as any agent"), "{text}");
}
