//! `another doctor` (docs/DOCTOR.md): read-only, one row per check, a fix for each problem,
//! exit 1 only when a check fails.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

struct Fx {
    _dir: tempfile::TempDir,
    /// Stands in for the user's home: client hook configs live under it.
    user: PathBuf,
    home: PathBuf,
}

impl Fx {
    fn new() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user");
        std::fs::create_dir_all(&user).unwrap();
        let home = dir.path().join("another-home");
        Fx {
            _dir: dir,
            user,
            home,
        }
    }

    fn cmd(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_another"))
            .args(args)
            .env("HOME", &self.user)
            .env("USERPROFILE", &self.user)
            .env("ANOTHER_HOME", &self.home)
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_LOG")
            .stdin(Stdio::null())
            .output()
            .expect("run another")
    }

    fn init(&self) {
        assert!(self.cmd(&["init"]).status.success());
    }

    /// `doctor --json` against a closed loopback port, so no real server is involved.
    fn doctor(&self) -> (Option<i32>, Value) {
        self.doctor_bind(&closed_port())
    }

    fn doctor_bind(&self, bind: &str) -> (Option<i32>, Value) {
        let out = self.cmd(&["--json", "doctor", "--bind", bind]);
        let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "{e}: {}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out.status.code(), v)
    }

    fn db(&self) -> PathBuf {
        self.home.join("another.db")
    }
}

fn row<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no {name} row in {report}"))
}

fn status(report: &Value, name: &str) -> String {
    row(report, name)["status"].as_str().unwrap().to_string()
}

fn closed_port() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    drop(l);
    addr
}

fn sha(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}

fn set_user_version(db: &Path, v: i64) {
    let c = rusqlite::Connection::open(db).unwrap();
    c.pragma_update(None, "user_version", v).unwrap();
}

fn user_version(db: &Path) -> i64 {
    let c = rusqlite::Connection::open(db).unwrap();
    c.pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap()
}

#[test]
fn a_missing_home_fails_and_is_not_created() {
    let fx = Fx::new();
    let (code, r) = fx.doctor();
    assert_eq!(code, Some(1), "{r}");
    assert_eq!(r["ok"], false);
    assert_eq!(status(&r, "home"), "fail");
    assert_eq!(row(&r, "home")["fix"], "another init");
    assert_eq!(status(&r, "database"), "skip");
    assert_eq!(status(&r, "peers"), "skip");
    assert!(!fx.home.exists(), "doctor created the home");
}

#[test]
fn a_home_without_a_database_fails_and_gets_none() {
    let fx = Fx::new();
    std::fs::create_dir_all(&fx.home).unwrap();
    let (code, r) = fx.doctor();
    assert_eq!(code, Some(1), "{r}");
    assert_eq!(status(&r, "database"), "fail");
    assert_eq!(row(&r, "database")["fix"], "another init");
    assert!(!fx.db().exists(), "doctor created a database");
    assert_eq!(std::fs::read_dir(&fx.home).unwrap().count(), 0);
}

#[test]
fn a_fresh_home_is_ok_and_doctor_changes_nothing() {
    let fx = Fx::new();
    fx.init();
    let before = sha(&fx.db());
    let version = user_version(&fx.db());
    let events_before = fx.cmd(&["--json", "events", "--limit", "1000"]).stdout;

    let (code, r) = fx.doctor();
    assert_eq!(code, Some(0), "{r}");
    assert_eq!(r["ok"], true);
    assert_eq!(status(&r, "database"), "ok", "{r}");
    assert!(row(&r, "database")["detail"]
        .as_str()
        .unwrap()
        .contains("(current)"));
    assert_eq!(status(&r, "hooks"), "info");
    assert_eq!(status(&r, "server"), "info");
    assert_eq!(status(&r, "peers"), "info");
    // Stable names, in order.
    let names: Vec<&str> = r["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["binary", "home", "database", "hooks", "server", "daemon", "peers"]
    );

    // Read-only: the database file, its schema and its events are as they were, and
    // nothing but SQLite's WAL sidecars may have appeared.
    assert_eq!(sha(&fx.db()), before);
    assert_eq!(user_version(&fx.db()), version);
    let events_after = fx.cmd(&["--json", "events", "--limit", "1000"]).stdout;
    assert_eq!(events_before, events_after);
    for entry in std::fs::read_dir(&fx.home).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(
            name.starts_with("another.db") || name == "hooks" || name == "logs",
            "unexpected {name} in the home"
        );
    }
}

#[test]
fn an_older_schema_warns_and_a_newer_one_fails_without_touching_the_file() {
    let fx = Fx::new();
    fx.init();
    let current = user_version(&fx.db());

    set_user_version(&fx.db(), current - 1);
    let before = sha(&fx.db());
    let (code, r) = fx.doctor();
    assert_eq!(code, Some(0), "{r}");
    assert_eq!(status(&r, "database"), "warn");
    assert!(row(&r, "database")["fix"]
        .as_str()
        .unwrap()
        .contains("another status"));
    assert_eq!(status(&r, "peers"), "skip");
    assert_eq!(sha(&fx.db()), before, "doctor migrated the database");
    assert_eq!(user_version(&fx.db()), current - 1);

    set_user_version(&fx.db(), current + 1);
    let (code, r) = fx.doctor();
    assert_eq!(code, Some(1), "{r}");
    assert_eq!(status(&r, "database"), "fail");
    assert_eq!(user_version(&fx.db()), current + 1);
}

#[test]
fn a_file_that_is_not_a_database_fails() {
    let fx = Fx::new();
    fx.init();
    std::fs::write(fx.db(), b"this is not sqlite, just some text long enough").unwrap();
    for side in ["another.db-wal", "another.db-shm"] {
        let _ = std::fs::remove_file(fx.home.join(side));
    }
    let (code, r) = fx.doctor();
    assert_eq!(code, Some(1), "{r}");
    assert_eq!(status(&r, "database"), "fail");
}

#[cfg(unix)]
#[test]
fn a_home_others_can_read_warns() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fx::new();
    fx.init();
    std::fs::set_permissions(&fx.home, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (code, r) = fx.doctor();
    assert_eq!(code, Some(0));
    assert_eq!(status(&r, "home"), "warn");
    assert!(row(&r, "home")["fix"]
        .as_str()
        .unwrap()
        .starts_with("chmod 700"));
}

#[cfg(unix)]
#[test]
fn a_hook_config_whose_wrapper_is_missing_warns() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fx::new();
    fx.init();
    let wrapper = fx.home.join("hooks").join("another-hook.sh");
    let settings = fx.user.join(".claude").join("settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(
        &settings,
        format!(
            r#"{{"hooks":{{"SessionStart":[{{"hooks":[{{"type":"command","command":"'{}' session-start --client claude"}}]}}]}}}}"#,
            wrapper.display()
        ),
    )
    .unwrap();

    let (_, r) = fx.doctor();
    assert_eq!(status(&r, "hooks"), "warn", "{r}");
    assert!(row(&r, "hooks")["fix"]
        .as_str()
        .unwrap()
        .contains("another hook install --client claude"));

    std::fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
    std::fs::write(&wrapper, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (_, r) = fx.doctor();
    assert_eq!(status(&r, "hooks"), "ok", "{r}");
    assert!(row(&r, "hooks")["detail"]
        .as_str()
        .unwrap()
        .starts_with("installed for claude"));
}

#[test]
fn a_hook_config_for_another_home_warns() {
    let fx = Fx::new();
    fx.init();
    let elsewhere = fx
        .user
        .join("other-home")
        .join("hooks")
        .join("another-hook.sh");
    std::fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
    std::fs::write(&elsewhere, "#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let settings = fx.user.join(".claude").join("settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    let quoted =
        serde_json::to_string(&format!("'{}' session-start", elsewhere.display())).unwrap();
    std::fs::write(&settings, format!(r#"{{"command":{quoted}}}"#)).unwrap();

    let (_, r) = fx.doctor();
    assert_eq!(status(&r, "hooks"), "warn", "{r}");
    assert!(row(&r, "hooks")["detail"]
        .as_str()
        .unwrap()
        .contains("another home"));
}

/// Answers one `GET /health` with 200, like `another serve`.
fn health_server() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        if let Ok((mut s, _)) = l.accept() {
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            let _ =
                s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok");
        }
    });
    addr
}

#[test]
fn the_server_row_reports_a_loopback_health_answer() {
    let fx = Fx::new();
    fx.init();
    let (_, r) = fx.doctor_bind(&health_server());
    assert_eq!(status(&r, "server"), "ok", "{r}");
    let (_, r) = fx.doctor();
    assert_eq!(status(&r, "server"), "info");
    // A non-loopback address is never contacted.
    let (_, r) = fx.doctor_bind("192.0.2.1:7466");
    assert_eq!(status(&r, "server"), "skip", "{r}");
}

#[test]
fn a_hub_not_synced_for_two_days_warns() {
    let fx = Fx::new();
    fx.init();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let two_days = 2 * 24 * 3600 * 1000;
    let c = rusqlite::Connection::open(fx.db()).unwrap();
    c.execute(
        "INSERT INTO nodes (node_id, display_name, role, is_local, created_at, public_key)
         VALUES ('hubnode', 'hub-box', 'hub', 0, ?1, 'AAAA')",
        [now - 3 * two_days],
    )
    .unwrap();
    c.execute(
        "INSERT INTO peers (node_id, role, url, perms, paired_at, last_sync_at)
         VALUES ('hubnode', 'hub', 'http://127.0.0.1:9', '', ?1, ?2)",
        [now - 3 * two_days, now - two_days],
    )
    .unwrap();
    drop(c);

    let (code, r) = fx.doctor();
    assert_eq!(code, Some(0), "warnings do not fail: {r}");
    assert_eq!(status(&r, "peers"), "warn", "{r}");
    assert!(row(&r, "peers")["detail"]
        .as_str()
        .unwrap()
        .contains("hub hub-box: last sync 48 h ago"));
}

#[test]
fn human_output_has_a_fix_line_under_problems() {
    let fx = Fx::new();
    let out = fx.cmd(&["doctor", "--bind", &closed_port()]);
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().collect();
    let home = lines
        .iter()
        .position(|l| l.starts_with("fail  home"))
        .unwrap();
    assert_eq!(lines[home + 1].trim(), "fix: another init");
}
