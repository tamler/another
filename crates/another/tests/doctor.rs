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
    assert_eq!(status(&r, "transports"), "info");
    // Stable names, in order.
    let names: Vec<&str> = r["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "binary",
            "home",
            "database",
            "hooks",
            "server",
            "daemon",
            "peers",
            "transports"
        ]
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
            name.starts_with("another.db")
                || name == "build.json"
                || name == "hooks"
                || name == "logs",
            "unexpected {name} in the home"
        );
    }
}

/// Every row of every table, read through one connection: a logical write by doctor that
/// leaves the main file alone (it lands in a live WAL) still shows here.
fn dump(conn: &rusqlite::Connection) -> Vec<String> {
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let mut out = Vec::new();
    for t in tables {
        let mut stmt = conn.prepare(&format!("SELECT * FROM \"{t}\"")).unwrap();
        let cols = stmt.column_count();
        let mut rows: Vec<String> = stmt
            .query_map([], |r| {
                let mut cells = Vec::new();
                for i in 0..cols {
                    let v: rusqlite::types::Value = r.get(i)?;
                    cells.push(format!("{v:?}"));
                }
                Ok(cells.join("|"))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        rows.sort();
        out.push(format!("{t}: {}", rows.join("; ")));
    }
    out.push(format!(
        "user_version {}",
        conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap()
    ));
    out
}

/// Review R1 of the spec: with a live WAL holding committed frames, the main file's hash
/// says nothing. Compare every table, and the hook configs, before and after.
#[cfg(unix)]
#[test]
fn doctor_changes_no_row_even_with_a_live_wal() {
    let fx = Fx::new();
    fx.init();
    assert!(fx
        .cmd(&["agent", "register", "w", "--program", "codex"])
        .status
        .success());
    // Hold a connection open and commit through it, so the frames stay in the WAL.
    let held = rusqlite::Connection::open(fx.db()).unwrap();
    held.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    held.execute("UPDATE agents SET description = 'in the wal'", [])
        .unwrap();
    let wal = fx.home.join("another.db-wal");
    assert!(std::fs::metadata(&wal).unwrap().len() > 0, "no WAL frames");
    let settings = fx.user.join(".claude").join("settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(&settings, r#"{"hooks":{}}"#).unwrap();

    let rows_before = dump(&held);
    let main_before = sha(&fx.db());
    let settings_before = sha(&settings);
    let (code, r) = fx.doctor();
    assert_eq!(code, Some(0), "{r}");
    assert_eq!(status(&r, "database"), "ok", "{r}");
    assert_eq!(dump(&held), rows_before);
    assert_eq!(sha(&fx.db()), main_before);
    assert_eq!(sha(&settings), settings_before);
}

/// The one network call follows no redirect: a `/health` answering 302 to a second
/// listener leaves that listener untouched.
#[test]
fn a_health_redirect_is_not_followed() {
    let fx = Fx::new();
    fx.init();
    let second = TcpListener::bind("127.0.0.1:0").unwrap();
    second.set_nonblocking(true).unwrap();
    let target = second.local_addr().unwrap();
    let first = TcpListener::bind("127.0.0.1:0").unwrap();
    let bind = first.local_addr().unwrap().to_string();
    let server = std::thread::spawn(move || {
        if let Ok((mut s, _)) = first.accept() {
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            let reply = format!(
                "HTTP/1.1 302 Found\r\nlocation: http://{target}/health\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            );
            let _ = s.write_all(reply.as_bytes());
        }
    });
    let (_, r) = fx.doctor_bind(&bind);
    server.join().unwrap();
    assert_eq!(status(&r, "server"), "warn", "{r}");
    assert!(
        second.accept().is_err(),
        "doctor followed the redirect to {target}"
    );
}

/// Only a literal loopback IP is contacted; a name is never looked up.
#[test]
fn a_named_bind_is_skipped_not_resolved() {
    let fx = Fx::new();
    fx.init();
    let (_, r) = fx.doctor_bind(&format!(
        "localhost:{}",
        closed_port().rsplit(':').next().unwrap()
    ));
    assert_eq!(status(&r, "server"), "skip", "{r}");
}

/// A home as an earlier build left it, one schema behind. `created_by` is the build its
/// record names; `None` is a home from before the record existed.
fn older_home(fx: &Fx, created_by: Option<&str>) -> i64 {
    fx.init();
    let current = user_version(&fx.db());
    std::fs::remove_dir_all(&fx.home).unwrap();
    std::fs::create_dir_all(&fx.home).unwrap();
    let c = rusqlite::Connection::open(fx.db()).unwrap();
    c.pragma_update(None, "journal_mode", "WAL").unwrap();
    another_core::apply_schema(&c, current - 1).unwrap();
    if let Some(build) = created_by {
        std::fs::write(
            fx.home.join("build.json"),
            format!(
                r#"{{"created":{{"version":"0.1.0","build":"{build}","schema":{},"at":"2026-11-01T00:00:00.000Z"}}}}"#,
                current - 1
            ),
        )
        .unwrap();
    }
    current
}

#[test]
fn an_older_schema_warns_and_a_newer_one_fails_without_touching_the_file() {
    let fx = Fx::new();
    let current = older_home(&fx, Some("development"));

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

/// The build and the database do not match: this test binary is a development build, and the
/// home is one it did not create (the 17 Nov 2026 failure). Doctor fails, says why, and does
/// not offer the migration a normal command would refuse.
#[test]
fn a_development_build_on_a_home_it_did_not_create_fails_with_the_way_out() {
    for created_by in [None, Some("release")] {
        let fx = Fx::new();
        let current = older_home(&fx, created_by);
        let before = sha(&fx.db());
        let (code, r) = fx.doctor();
        assert_eq!(code, Some(1), "{r}");
        assert_eq!(status(&r, "database"), "fail", "{r}");
        let detail = row(&r, "database")["detail"].as_str().unwrap().to_string();
        let fix = row(&r, "database")["fix"].as_str().unwrap().to_string();
        assert!(
            detail.contains(&format!("schema v{}", current - 1))
                && detail.contains("development build")
                && detail.contains("did not create"),
            "{detail}"
        );
        assert!(
            fix.contains("installed another") && fix.contains("--dev-migrate"),
            "{fix}"
        );
        assert_eq!(status(&r, "peers"), "skip");
        assert_eq!(sha(&fx.db()), before, "doctor changed the database");
        assert!(!fx.home.join("backups").exists(), "doctor took a backup");
    }
}

/// A database whose version this build knows but whose tables another build changed.
#[test]
fn a_schema_another_build_changed_fails() {
    let fx = Fx::new();
    fx.init();
    let current = user_version(&fx.db());
    rusqlite::Connection::open(fx.db())
        .unwrap()
        .execute_batch("ALTER TABLE agents ADD COLUMN stray TEXT")
        .unwrap();
    let before = sha(&fx.db());
    let (code, r) = fx.doctor();
    assert_eq!(code, Some(1), "{r}");
    assert_eq!(status(&r, "database"), "fail", "{r}");
    let detail = row(&r, "database")["detail"].as_str().unwrap().to_string();
    assert!(
        detail.contains(&format!(
            "schema v{current} does not match this build's v{current}"
        )),
        "{detail}"
    );
    assert!(row(&r, "database")["fix"]
        .as_str()
        .unwrap()
        .contains("restore"));
    assert_eq!(sha(&fx.db()), before, "doctor changed the database");
}

/// A database a newer build migrated: doctor names the build that did it and the backup
/// that undoes it.
#[test]
fn a_newer_schema_names_the_build_that_migrated_it_and_the_backup() {
    let fx = Fx::new();
    let current = older_home(&fx, None);
    let out = fx.cmd(&["--dev-migrate", "status"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(user_version(&fx.db()), current);
    let backup = std::fs::read_dir(fx.home.join("backups"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();

    // Seen from the installed build, one schema behind the build that migrated the home:
    // the database and its record both name the newer schema.
    set_user_version(&fx.db(), current + 1);
    let record = fx.home.join("build.json");
    let mut origin: Value = serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    assert_eq!(origin["migrated"]["schema"], current);
    origin["migrated"]["schema"] = (current + 1).into();
    std::fs::write(&record, origin.to_string()).unwrap();
    let (code, r) = fx.doctor();
    assert_eq!(code, Some(1), "{r}");
    assert_eq!(status(&r, "database"), "fail", "{r}");
    let detail = row(&r, "database")["detail"].as_str().unwrap().to_string();
    let fix = row(&r, "database")["fix"].as_str().unwrap().to_string();
    assert!(
        detail.contains("is newer than this build")
            && detail.contains("last migrated by a development build of another"),
        "{detail}"
    );
    assert!(
        fix.contains("upgrade another") && fix.contains(&backup.display().to_string()),
        "{fix}"
    );
}

/// The backup doctor offers is a backup: not whatever file is newest in `backups`, and not
/// one whose schema this build cannot open either.
#[test]
fn the_backup_hint_names_only_a_real_backup() {
    let fx = Fx::new();
    let current = older_home(&fx, None);
    assert!(fx.cmd(&["--dev-migrate", "status"]).status.success());
    let dir = fx.home.join("backups");
    let real = std::fs::read_dir(&dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::write(dir.join("notes.txt"), "kept here by hand").unwrap();
    std::fs::create_dir(dir.join("another-v1-20260101T000000Z.db")).unwrap();
    std::fs::write(
        dir.join(format!("another-v{}-20270101T000000Z.db", current + 5)),
        "from a newer build",
    )
    .unwrap();

    set_user_version(&fx.db(), current + 1);
    let (_, r) = fx.doctor();
    let fix = row(&r, "database")["fix"].as_str().unwrap().to_string();
    assert!(fix.contains(&real.display().to_string()), "{fix}");
    assert!(
        !fix.contains("notes.txt") && !fix.contains("20270101"),
        "{fix}"
    );

    // With no real backup there, doctor names none.
    std::fs::remove_file(&real).unwrap();
    let (_, r) = fx.doctor();
    let fix = row(&r, "database")["fix"].as_str().unwrap().to_string();
    assert!(fix.contains("restore a backup (docs/DOCTOR.md)"), "{fix}");
    assert!(!fix.contains("backups/"), "{fix}");
}

/// An empty database file is created into, not migrated: doctor must not call it one this
/// development build refuses, because a normal command does not refuse it.
#[test]
fn an_empty_database_is_not_called_unmigratable() {
    let fx = Fx::new();
    std::fs::create_dir_all(&fx.home).unwrap();
    drop(rusqlite::Connection::open(fx.db()).unwrap());
    std::fs::write(fx.db(), b"").unwrap();
    let (code, r) = fx.doctor();
    assert_eq!(code, Some(0), "{r}");
    assert_eq!(status(&r, "database"), "warn", "{r}");
    let detail = row(&r, "database")["detail"].as_str().unwrap().to_string();
    assert!(detail.contains("empty database"), "{detail}");
    assert!(!detail.contains("will not migrate"), "{detail}");
    assert_eq!(row(&r, "database")["fix"], "another init");
    assert!(fx.cmd(&["status"]).status.success());
    assert!(user_version(&fx.db()) > 0);
}

/// With `--dev-migrate`, a normal command migrates the home, so doctor says so too.
#[test]
fn doctor_honours_dev_migrate() {
    let fx = Fx::new();
    older_home(&fx, None);
    let (_, r) = fx.doctor();
    assert_eq!(status(&r, "database"), "fail", "{r}");

    let bind = closed_port();
    let out = fx.cmd(&["--dev-migrate", "--json", "doctor", "--bind", &bind]);
    let r: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{r}");
    assert_eq!(status(&r, "database"), "warn", "{r}");
    assert!(row(&r, "database")["fix"]
        .as_str()
        .unwrap()
        .contains("another status"));
}

/// A negative schema version is not an older another schema: no build migrates it, so doctor
/// must not advise a normal command (which refuses or fails on it). It fails and points at the
/// home instead, and leaves the file as it was.
#[test]
fn a_negative_schema_version_fails_and_is_not_offered_a_migration() {
    let fx = Fx::new();
    fx.init();
    // `user_version` is a signed 32-bit value: test past -1 down to its minimum.
    for v in [-1, -2, i64::from(i32::MIN)] {
        set_user_version(&fx.db(), v);
        let before = sha(&fx.db());
        let (code, r) = fx.doctor();
        assert_eq!(code, Some(1), "{v}: {r}");
        assert_eq!(status(&r, "database"), "fail", "{v}: {r}");
        let detail = row(&r, "database")["detail"].as_str().unwrap().to_string();
        let fix = row(&r, "database")["fix"].as_str().unwrap().to_string();
        assert!(detail.contains(&format!("{v} is not valid")), "{detail}");
        assert!(
            !detail.contains("migrates") && !fix.contains("migrates"),
            "{v}: {r}"
        );
        assert!(fix.contains("--home or ANOTHER_HOME"), "{fix}");
        assert_eq!(sha(&fx.db()), before, "doctor changed the database");
        assert_eq!(user_version(&fx.db()), v);
    }
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

/// Adversarial pass (claude@another-wake-e2e-20261116): a server on the default port that
/// serves **another home** (another node id in its `/health`) is not this installation's
/// server. MCP clients pointed at it write to a different database than the hooks read, the
/// split doctor exists to catch. It must not read as `ok`.
#[test]
fn adv158_a_server_for_another_node_is_not_ok() {
    let fx = Fx::new();
    fx.init();
    let (_, r) = fx.doctor_bind(&health_json(&format!(
        r#"{{"ok":true,"node_id":"{OTHER}","auth":false,"dashboard":"local"}}"#
    )));
    assert_ne!(
        status(&r, "server"),
        "ok",
        "a server for another node id passes as this home's server: {r}"
    );
    let detail = row(&r, "server")["detail"].as_str().unwrap();
    assert!(detail.contains(&format!("serves node {OTHER}")), "{r}");
    // Not this home's server, so the daemon row does not take it as an answer either.
    assert!(row(&r, "server")["fix"].as_str().is_some(), "{r}");

    // This home's own node, or an older server that does not say, is ok.
    let ours: String = rusqlite::Connection::open(fx.db())
        .unwrap()
        .query_row("SELECT node_id FROM nodes WHERE is_local = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    let (_, r) = fx.doctor_bind(&health_json(&format!(
        r#"{{"ok":true,"node_id":"{ours}"}}"#
    )));
    assert_eq!(status(&r, "server"), "ok", "{r}");
    let (_, r) = fx.doctor_bind(&health_json(r#"{"ok":true}"#));
    assert_eq!(status(&r, "server"), "ok", "{r}");
}

const OTHER: &str = "01OTHERNODE000000000000000";

/// Answers one `GET /health` with 200 and `body` as JSON.
fn health_json(body: &str) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        if let Ok((mut s, _)) = l.accept() {
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(head.as_bytes());
            let _ = s.write_all(body.as_bytes());
        }
    });
    addr
}
