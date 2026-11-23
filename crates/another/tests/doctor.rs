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
        {
            let mut legacy_fixture_command = Command::new(env!("CARGO_BIN_EXE_another"));
            legacy_fixture_command.arg("--unguarded-repair");
            legacy_fixture_command
        }
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
            "transports",
            "resources",
            "listeners"
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
#[cfg(unix)]
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
    let out = fx.cmd(&["--dev-migrate", "init"]);
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
    assert!(fx.cmd(&["--dev-migrate", "init"]).status.success());
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
    assert!(fx.cmd(&["init"]).status.success());
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

/// The lapsed resources and unregistered listeners rows (docs/DOCTOR.md, "The orphan
/// scan"). The scan's programs are found on `PATH`, so a test that needs a fixed answer
/// puts its own `lsof` first.
#[cfg(unix)]
mod orphan_scan {
    use super::*;
    use std::io::BufRead;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Child;

    impl Fx {
        fn ok(&self, args: &[&str]) -> Value {
            let mut all = vec!["--json"];
            all.extend_from_slice(args);
            let out = self.cmd(&all);
            assert!(
                out.status.success(),
                "another {args:?}: {}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            serde_json::from_slice(&out.stdout).unwrap()
        }

        /// A `long_job` resource with no port and no pid; returns its id.
        fn job(&self, name: &str) -> String {
            let reg = self.ok(&["resource", "register", "long_job", name, "--agent", "alice"]);
            reg["resource"]["resource_id"].as_str().unwrap().to_string()
        }

        /// `doctor` with `PATH` set to `path`.
        fn doctor_on_path(&self, path: &str, json: bool) -> Output {
            let bind = closed_port();
            let mut args = vec!["doctor", "--bind", &bind];
            if json {
                args.insert(0, "--json");
            }
            self.on_path(path, &args)
        }

        /// Any command with `PATH` set to `path`.
        fn on_path(&self, path: &str, args: &[&str]) -> Output {
            {
                let mut legacy_fixture_command = Command::new(env!("CARGO_BIN_EXE_another"));
                legacy_fixture_command.arg("--unguarded-repair");
                legacy_fixture_command
            }
            .args(args)
            .env("HOME", &self.user)
            .env("ANOTHER_HOME", &self.home)
            .env("PATH", path)
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_LOG")
            .stdin(Stdio::null())
            .output()
            .expect("run another")
        }

        fn doctor_json_on_path(&self, path: &str) -> (Option<i32>, Value) {
            let out = self.doctor_on_path(path, true);
            let v = serde_json::from_slice(&out.stdout)
                .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)));
            (out.status.code(), v)
        }

        /// A directory holding an `lsof` that prints `fields`, and a `PATH` that finds it
        /// before the system's programs (`ps` stays the real one).
        fn fake_lsof(&self, fields: &str) -> String {
            let bin = self.user.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let listing = bin.join("listing");
            std::fs::write(&listing, fields).unwrap();
            let lsof = bin.join("lsof");
            std::fs::write(
                &lsof,
                format!("#!/bin/sh\nexec /bin/cat '{}'\n", listing.display()),
            )
            .unwrap();
            std::fs::set_permissions(&lsof, std::fs::Permissions::from_mode(0o755)).unwrap();
            format!("{}:/usr/bin:/bin", bin.display())
        }

        fn with_agent() -> Fx {
            let fx = Fx::new();
            fx.init();
            fx.ok(&["agent", "register", "alice", "--program", "claude"]);
            fx
        }
    }

    fn detail(report: &Value, name: &str) -> String {
        row(report, name)["detail"].as_str().unwrap().to_string()
    }

    fn files(home: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(home)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// This test binary again, listening on a loopback port until its input closes.
    fn listening_child() -> (Child, u16) {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "orphan_scan::listening_fixture", "--nocapture"])
            .env("ANOTHER_DOCTOR_TEST_CHILD", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let out = std::io::BufReader::new(child.stdout.take().unwrap());
        let port = out
            .lines()
            .map_while(Result::ok)
            .find_map(|l| l.split("listening ").nth(1)?.trim().parse().ok())
            .expect("the child's port");
        (child, port)
    }

    /// Ends the child by closing its input, then waits for it. Its exit status says
    /// nothing: its output pipe was closed once the port was read.
    fn stop(mut child: Child) {
        drop(child.stdin.take());
        child.wait().unwrap();
    }

    #[test]
    fn listening_fixture() {
        if std::env::var_os("ANOTHER_DOCTOR_TEST_CHILD").is_none() {
            return;
        }
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        println!("listening {}", l.local_addr().unwrap().port());
        std::io::copy(&mut std::io::stdin(), &mut std::io::sink()).unwrap();
    }

    /// The done condition of Q5: a home with a lapsed resource and an orphan listener.
    /// Doctor lists both and the home is as it was, including two leases that ran out by
    /// the clock and that no sweep has expired. The registry counts this node's own lease
    /// until a sweep (`quiet-9` is not listed yet) and another node's only until its expiry
    /// (`train-7` is lapsed, with no time recorded).
    #[test]
    fn doctor_lists_a_lapsed_resource_and_an_orphan_listener_and_changes_nothing() {
        let fx = Fx::with_agent();
        let released = fx.job("eval-run");
        fx.ok(&["release", &released, "--agent", "alice"]);
        let unswept = fx.job("train-7");
        let quiet = fx.job("quiet-9");
        fx.job("still-live");
        let held = rusqlite::Connection::open(fx.db()).unwrap();
        held.execute(
            "INSERT INTO nodes (node_id, display_name, created_at) VALUES (?1, 'other-box', 1)",
            [OTHER],
        )
        .unwrap();
        held.execute(
            "UPDATE resource_leases SET expires_at = 1000, node_id = ?2 WHERE resource_id = ?1",
            [&unswept, OTHER],
        )
        .unwrap();
        held.execute(
            "UPDATE resource_leases SET expires_at = 1000 WHERE resource_id = ?1",
            [&quiet],
        )
        .unwrap();
        let (child, port) = listening_child();
        let path = fx.fake_lsof(&format!(
            "p{}\ncfixture\nLme\nn127.0.0.1:{port}\n",
            child.id()
        ));

        let rows_before = dump(&held);
        let files_before = files(&fx.home);
        let (code, r) = fx.doctor_json_on_path(&path);
        let text = fx.doctor_on_path(&path, false);
        assert_eq!(dump(&held), rows_before, "doctor wrote to the database");
        assert_eq!(files(&fx.home), files_before);
        let open: i64 = held
            .query_row(
                "SELECT count(*) FROM resource_leases
                 WHERE resource_id IN (?1, ?2) AND released_at IS NULL AND expires_at = 1000",
                [&unswept, &quiet],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(open, 2, "the leases that ran out are still unswept");
        assert!(child_running(&child), "doctor stopped the listener");
        stop(child);

        assert_eq!(code, Some(0), "{r}");
        assert_eq!(r["ok"], true);
        assert_eq!(status(&r, "resources"), "warn", "{r}");
        let lapsed = detail(&r, "resources");
        assert!(lapsed.starts_with("2 lapsed: "), "{lapsed}");
        assert!(lapsed.contains("long_job eval-run (for "), "{lapsed}");
        assert!(
            lapsed.contains("long_job train-7 (not recorded by a sweep yet)"),
            "{lapsed}"
        );
        assert!(!lapsed.contains("still-live"), "{lapsed}");
        assert!(!lapsed.contains("quiet-9"), "{lapsed}");
        assert_eq!(row(&r, "resources")["fix"], "another orphans");

        assert_eq!(status(&r, "listeners"), "info", "{r}");
        let listeners = detail(&r, "listeners");
        assert!(listeners.starts_with("1 unregistered"), "{listeners}");
        // The child's real program name, from `ps`: this test binary.
        assert!(
            listeners.contains(&format!("port {port} doctor-")),
            "{listeners}"
        );
        assert!(!listeners.contains('/'), "{listeners}");
        assert_eq!(row(&r, "listeners")["fix"], "another orphans");

        assert_eq!(text.status.code(), Some(0));
        let text = String::from_utf8_lossy(&text.stdout).into_owned();
        assert!(text.contains("\nwarn  resources 2 lapsed: "), "{text}");
        assert!(text.contains("\ninfo  listeners 1 unregistered"), "{text}");
        assert_eq!(text.matches("fix: another orphans\n").count(), 2, "{text}");
    }

    fn child_running(child: &Child) -> bool {
        another::scan::alive(child.id())
    }

    /// The same scan with the system's own `lsof` or `ss`: a listener this test holds is
    /// counted. Which five are named depends on what else the machine runs.
    #[test]
    fn the_real_scanner_counts_a_listener_this_test_holds() {
        let fx = Fx::with_agent();
        let (child, port) = listening_child();
        let (code, r) = fx.doctor();
        stop(child);
        assert_eq!(code, Some(0), "{r}");
        assert_eq!(status(&r, "listeners"), "info", "{r}");
        let listeners = detail(&r, "listeners");
        if listeners.contains("did not run") {
            eprintln!("no lsof or ss on this machine: {listeners}");
            return;
        }
        let count: usize = listeners.split(' ').next().unwrap().parse().unwrap();
        assert!(count >= 1, "{listeners}");
        if count <= 5 {
            assert!(listeners.contains(&format!("port {port} ")), "{listeners}");
        }
    }

    #[test]
    fn nothing_lapsed_and_no_listener_are_two_quiet_rows() {
        let fx = Fx::with_agent();
        fx.job("still-live");
        let (code, r) = fx.doctor_json_on_path(&fx.fake_lsof(""));
        assert_eq!(code, Some(0), "{r}");
        assert_eq!(status(&r, "resources"), "ok", "{r}");
        assert_eq!(detail(&r, "resources"), "none lapsed");
        assert!(row(&r, "resources")["fix"].is_null());
        assert_eq!(status(&r, "listeners"), "ok", "{r}");
        assert_eq!(detail(&r, "listeners"), "none unregistered (lsof)");
        assert!(row(&r, "listeners")["fix"].is_null());
    }

    /// A registered port is not an orphan, and Another's own listeners and system
    /// services are left out, as in `another orphans`.
    #[test]
    fn registered_and_system_listeners_are_not_counted() {
        let fx = Fx::with_agent();
        fx.ok(&[
            "resource",
            "register",
            "dev_server",
            "vite@5173",
            "--agent",
            "alice",
        ]);
        let path = fx.fake_lsof(
            "p4000000001\ncnode\nLme\nn*:5173\np4000000002\ncsshd\nLroot\nn*:22\n\
             p4000000003\ncanother\nLme\nn127.0.0.1:7466\n",
        );
        let (_, r) = fx.doctor_json_on_path(&path);
        assert_eq!(detail(&r, "listeners"), "none unregistered (lsof)", "{r}");
    }

    /// More than five of either: the count is the whole number, five are named.
    #[test]
    fn only_five_of_each_are_named() {
        let fx = Fx::with_agent();
        let mut fields = String::new();
        for n in 1..=7 {
            let id = fx.job(&format!("job-{n}"));
            fx.ok(&["release", &id, "--agent", "alice"]);
            fields.push_str(&format!("p400000000{n}\ncsrv{n}\nLme\nn*:900{n}\n"));
        }
        let (code, r) = fx.doctor_json_on_path(&fx.fake_lsof(&fields));
        assert_eq!(code, Some(0), "{r}");
        let lapsed = detail(&r, "resources");
        assert!(lapsed.starts_with("7 lapsed: "), "{lapsed}");
        assert_eq!(lapsed.matches("long_job job-").count(), 5, "{lapsed}");
        assert!(lapsed.ends_with(", and 2 more"), "{lapsed}");
        let listeners = detail(&r, "listeners");
        assert!(listeners.starts_with("7 unregistered"), "{listeners}");
        assert_eq!(listeners.matches("port 900").count(), 5, "{listeners}");
        assert!(listeners.ends_with(", and 2 more"), "{listeners}");
    }

    /// Without `lsof`, `ss` and `ps`, doctor still runs, lists the lapsed resource, says
    /// the listener scan did not run, and exits 0.
    #[test]
    fn a_missing_scanner_is_said_plainly_and_fails_nothing() {
        let fx = Fx::with_agent();
        let id = fx.job("eval-run");
        fx.ok(&["release", &id, "--agent", "alice"]);
        let empty = fx.user.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let (code, r) = fx.doctor_json_on_path(&empty.to_string_lossy());
        assert_eq!(code, Some(0), "{r}");
        assert_eq!(r["ok"], true);
        assert_eq!(status(&r, "resources"), "warn", "{r}");
        assert!(detail(&r, "resources").contains("long_job eval-run"), "{r}");
        assert_eq!(status(&r, "listeners"), "info", "{r}");
        let listeners = detail(&r, "listeners");
        assert!(
            listeners.starts_with("the listener scan did not run: lsof and ss were tried"),
            "{listeners}"
        );
    }

    /// Names other agents wrote and command names other processes chose reach the terminal
    /// as plain text on one line.
    #[test]
    fn hostile_names_are_printed_as_one_safe_line() {
        let fx = Fx::with_agent();
        let id = fx.job("placeholder");
        fx.ok(&["release", &id, "--agent", "alice"]);
        let hostile = "x\u{1b}[2J\r\nfail  database gone\u{202e}\u{7}";
        rusqlite::Connection::open(fx.db())
            .unwrap()
            .execute(
                "UPDATE resources SET name = ?1 WHERE resource_id = ?2",
                [hostile, &id],
            )
            .unwrap();
        // No such process, so `ps` has nothing and the name `lsof` gave is what is shown.
        let path = fx.fake_lsof("p4000000001\ncev\u{1b}[31mil\u{9b}\u{2066}\tsrv\nLme\nn*:9001\n");
        let (code, r) = fx.doctor_json_on_path(&path);
        assert_eq!(code, Some(0), "{r}");
        for name in ["resources", "listeners"] {
            let shown = detail(&r, name);
            assert!(
                !shown.chars().any(another_core::is_unsafe_text_char),
                "{name}: {shown:?}"
            );
        }
        assert!(detail(&r, "resources").contains("fail  database gone"));
        assert!(detail(&r, "listeners").contains("port 9001 ev [31mil"));

        let text = fx.doctor_on_path(&path, false);
        let text = String::from_utf8_lossy(&text.stdout).into_owned();
        assert!(!text.contains('\u{1b}'), "{text:?}");
        let rows = r["checks"].as_array().unwrap().len();
        let fixes = text
            .lines()
            .filter(|l| l.trim_start().starts_with("fix: "))
            .count();
        assert_eq!(
            text.lines().count(),
            rows + fixes,
            "an extra line: {text:?}"
        );
    }

    /// A resource another node owns is that node's to deal with (D25): not listed here.
    #[test]
    fn another_nodes_lapsed_resource_is_not_listed() {
        let fx = Fx::with_agent();
        let id = fx.job("theirs");
        fx.ok(&["release", &id, "--agent", "alice"]);
        let c = rusqlite::Connection::open(fx.db()).unwrap();
        c.execute(
            "INSERT INTO nodes (node_id, display_name, created_at) VALUES (?1, 'other-box', 1)",
            [OTHER],
        )
        .unwrap();
        c.execute(
            "UPDATE resources SET node_id = ?1 WHERE resource_id = ?2",
            [OTHER, &id],
        )
        .unwrap();
        drop(c);
        let (_, r) = fx.doctor_json_on_path(&fx.fake_lsof(""));
        assert_eq!(status(&r, "resources"), "ok", "{r}");
        assert_eq!(detail(&r, "resources"), "none lapsed");
    }

    /// Review R1, P2-1: `lsof` output that is not field output (a line starting with a
    /// multi-byte character, empty lines, a lone tag, a very long line) is read as no
    /// listener. Doctor keeps every row and its exit status, and `another orphans`, which
    /// reads the same output, does not stop either.
    #[test]
    fn garbage_from_lsof_costs_no_row_and_no_exit_status() {
        let fx = Fx::with_agent();
        let id = fx.job("eval-run");
        fx.ok(&["release", &id, "--agent", "alice"]);
        let long = "\u{e9}".repeat(100_000);
        let path = fx.fake_lsof(&format!(
            "\u{e9}x\n\n\np\nn\n\u{1f600}\n{long}\nn{long}:80\np{long}\nc\u{e9}\nn\u{e9}:\u{e9}\n"
        ));

        let (code, r) = fx.doctor_json_on_path(&path);
        assert_eq!(code, Some(0), "{r}");
        assert_eq!(r["checks"].as_array().unwrap().len(), 10, "{r}");
        assert_eq!(status(&r, "resources"), "warn", "{r}");
        assert_eq!(detail(&r, "listeners"), "none unregistered (lsof)", "{r}");
        let text = fx.doctor_on_path(&path, false);
        assert_eq!(text.status.code(), Some(0));
        assert!(String::from_utf8_lossy(&text.stdout).contains("\nok    listeners none"));

        let orphans = fx.on_path(&path, &["--json", "orphans"]);
        assert_eq!(
            orphans.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&orphans.stderr)
        );
        let listed: Value = serde_json::from_slice(&orphans.stdout).unwrap();
        assert_eq!(listed["orphans"].as_array().unwrap().len(), 1, "{listed}");
        assert_eq!(listed["orphans"][0]["source"], "lapsed_lease");
    }

    /// Review R1, P2-2: doctor contacts nothing but loopback `/health`. `another resources`
    /// tries a connection to a live resource another node owns, at whatever endpoint that
    /// node registered; doctor's read must not. The endpoint here is a listener this test
    /// holds, and nothing may arrive on it.
    #[test]
    fn doctor_does_not_connect_to_another_nodes_resource() {
        let fx = Fx::with_agent();
        let theirs = TcpListener::bind("127.0.0.1:0").unwrap();
        theirs.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", theirs.local_addr().unwrap());
        let reg = fx.ok(&[
            "resource",
            "register",
            "dev_server",
            "theirs",
            "--agent",
            "alice",
            "--endpoint",
            &endpoint,
        ]);
        let id = reg["resource"]["resource_id"].as_str().unwrap();
        let c = rusqlite::Connection::open(fx.db()).unwrap();
        c.execute(
            "INSERT INTO nodes (node_id, display_name, created_at) VALUES (?1, 'other-box', 1)",
            [OTHER],
        )
        .unwrap();
        c.execute(
            "UPDATE resources SET node_id = ?1 WHERE resource_id = ?2",
            [OTHER, id],
        )
        .unwrap();
        drop(c);
        let path = fx.fake_lsof("");
        let untouched = |after: &str| match theirs.accept() {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            other => panic!("{after} reached the other node's endpoint: {other:?}"),
        };

        let (code, r) = fx.doctor_json_on_path(&path);
        assert_eq!(code, Some(0), "{r}");
        assert_eq!(status(&r, "resources"), "ok", "{r}");
        untouched("doctor --json");
        assert_eq!(fx.doctor_on_path(&path, false).status.code(), Some(0));
        untouched("doctor");

        // The fixture is one the probe would reach: `another resources` does connect.
        let listed = fx.ok(&["resources"]);
        assert_eq!(listed[0]["node"], "other-box", "{listed}");
        assert_eq!(listed[0]["status"], "live", "{listed}");
        assert_eq!(listed[0]["reachable"], true, "{listed}");
        assert!(theirs.accept().is_ok(), "the probe never arrived");
    }
}

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
