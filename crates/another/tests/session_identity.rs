use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::json;

#[test]
fn missing_identity_denies_and_lifecycle_does_not_persist_raw_id() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("another-home");
    let root = dir.path().join("work");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("test.rs"), "before").unwrap();
    let root = std::fs::canonicalize(root).unwrap();
    let run = |args: &[&str], input: Option<String>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_another"));
        command
            .args(args)
            .env("ANOTHER_HOME", &home)
            .env("HOME", dir.path())
            .env_remove("ANOTHER_AGENT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        if let Some(input) = input {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
        }
        child.wait_with_output().unwrap()
    };
    assert!(run(&["init"], None).status.success());
    let raw = "raw-private-session-identity-392";
    let event = json!({"cwd":root,"session_id":raw});
    let start = run(
        &["hook", "run", "session-start", "--client", "claude"],
        Some(event.to_string()),
    );
    assert!(start.status.success());
    let digest = String::from_utf8(start.stdout).unwrap();
    assert!(!digest.contains(raw));
    let actor = digest
        .strip_prefix("Another: ")
        .unwrap()
        .split(' ')
        .next()
        .unwrap();
    let project = root.to_str().unwrap();
    assert!(run(
        &[
            "hold",
            "acquire",
            "--agent",
            actor,
            "--project",
            project,
            "test.rs"
        ],
        None,
    )
    .status
    .success());
    let base_input = json!({"cwd": root, "session_id": raw,"tool_use_id":"alias-call","tool_name":"mcp__another__controlled_edit",
        "tool_input":{"project":root,"path":"test.rs","expected_sha256":another_core::controlled_sha256(b"before"),"replacement":"after"}});
    let mut equal_alias = base_input.clone();
    equal_alias["sessionId"] = json!(raw);
    equal_alias["conversation_id"] = json!(raw);
    let allowed = run(
        &["hook", "run", "pre-controlled-edit", "--client", "claude"],
        Some(equal_alias.to_string()),
    );
    assert!(allowed.status.success());
    assert_eq!(allowed.stdout, b"");
    for bad_value in [
        json!("different-session"),
        json!(null),
        json!(42),
        json!({}),
    ] {
        let mut conflicting = base_input.clone();
        conflicting["sessionId"] = bad_value;
        let blocked = run(
            &["hook", "run", "pre-native-edit", "--client", "claude"],
            Some(conflicting.to_string()),
        );
        assert!(blocked.status.success());
        assert!(String::from_utf8_lossy(&blocked.stdout).contains("deny"));
    }
    assert!(run(
        &["wake", "set", "--agent", actor, "--via", "herdr", "--target", "w1:p9"],
        None,
    )
    .status
    .success());
    let first_holds = run(&["--json", "holds"], None);
    assert!(first_holds.status.success());
    let first_holds: serde_json::Value = serde_json::from_slice(&first_holds.stdout).unwrap();
    assert_eq!(
        first_holds.as_array().unwrap().len(),
        2,
        "manual and pending coverage are separate"
    );
    for name in ["session-start", "prompt", "post-tool"] {
        let result = run(
            &["hook", "run", name, "--client", "claude"],
            Some(event.to_string()),
        );
        assert!(result.status.success());
    }
    let child = run(
        &[
            "session",
            "exec",
            "--agent",
            actor,
            "--",
            env!("CARGO_BIN_EXE_another"),
            "--json",
            "inbox",
        ],
        None,
    );
    assert!(
        child.status.success(),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    assert!(String::from_utf8_lossy(&child.stdout).contains(actor));
    let bare = actor.split('@').next().unwrap();
    let shorthand = run(
        &[
            "session",
            "exec",
            "--agent",
            bare,
            "--",
            env!("CARGO_BIN_EXE_another"),
            "--json",
            "inbox",
        ],
        None,
    );
    assert!(shorthand.status.success());
    assert!(String::from_utf8_lossy(&shorthand.stdout).contains(actor));
    #[cfg(unix)]
    {
        let env_output = run(
            &["session", "exec", "--agent", bare, "--", "/usr/bin/env"],
            None,
        );
        assert!(env_output.status.success());
        assert!(String::from_utf8_lossy(&env_output.stdout)
            .lines()
            .any(|line| line == format!("ANOTHER_AGENT={actor}")));
        let failed = run(
            &["session", "exec", "--agent", actor, "--", "/usr/bin/false"],
            None,
        );
        assert_eq!(failed.status.code(), Some(1));
    }
    let mismatch = run(
        &[
            "session",
            "exec",
            "--agent",
            &format!("{bare}@wrong"),
            "--",
            env!("CARGO_BIN_EXE_another"),
            "--version",
        ],
        None,
    );
    assert!(!mismatch.status.success());
    let end = run(
        &["hook", "run", "session-end", "--client", "claude"],
        Some(event.to_string()),
    );
    assert!(end.status.success());
    assert!(!String::from_utf8_lossy(&end.stdout).contains(raw));
    assert!(!String::from_utf8_lossy(&end.stderr).contains(raw));
    let after_holds = run(&["--json", "holds"], None);
    assert!(after_holds.status.success());
    let after_holds: serde_json::Value = serde_json::from_slice(&after_holds.stdout).unwrap();
    assert_eq!(
        after_holds, first_holds,
        "passive lifecycle hooks must not change either hold"
    );
    let targets = run(&["--json", "wake", "list"], None);
    assert!(targets.status.success());
    let targets: serde_json::Value = serde_json::from_slice(&targets.stdout).unwrap();
    assert_eq!(targets[0]["target"], "w1:p9");
    let input = json!({"cwd":root,"tool_name":"Edit","tool_input":{"file_path":"test.rs"}});
    let out = run(
        &["hook", "run", "pre-native-edit", "--client", "claude"],
        Some(input.to_string()),
    );
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("deny"));
    let events = run(&["--json", "events"], None);
    assert!(events.status.success());
    let output = String::from_utf8(events.stdout).unwrap();
    assert!(!output.contains(raw));
    assert!(!output.contains("session.started"));
    assert!(!output.contains("session.ended"));
    assert!(run(&["heartbeat", "--agent", actor, "--start"], None)
        .status
        .success());
    let events = run(&["--json", "events"], None);
    assert!(events.status.success());
    let output = String::from_utf8(events.stdout).unwrap();
    assert!(!output.contains(raw));
    assert!(output.contains("session_fingerprint"));
}

#[cfg(unix)]
#[test]
fn old_home_requires_explicit_fenced_upgrade_and_verified_wal_backup() {
    use rusqlite::Connection;
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let backup = dir.path().join("backup");
    let command = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_another"))
            .args(args)
            .env("ANOTHER_HOME", &home)
            .env("HOME", dir.path())
            .env_remove("ANOTHER_AGENT")
            .output()
            .unwrap()
    };
    assert!(command(&["init"]).status.success());
    let db = home.join("another.db");
    let old = Connection::open(&db).unwrap();
    // The marker lives in a catalog table: a table the schema does not define is refused.
    old.execute_batch("DROP TABLE admission_holds; DROP TABLE admissions; DROP TABLE admission_secret; DROP TABLE announcements; PRAGMA user_version=7; UPDATE nodes SET display_name='committed-wal';").unwrap();
    let denied = command(&["status"]);
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("offline upgrade"));
    let mut hook = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["hook", "run", "session-start", "--client", "claude"])
        .env("ANOTHER_HOME", &home)
        .env("HOME", dir.path())
        .env_remove("ANOTHER_AGENT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    hook.stdin
        .take()
        .unwrap()
        .write_all(
            json!({"cwd": dir.path(), "session_id":"old-client-test"})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
    assert!(hook.wait_with_output().unwrap().status.success());
    assert_eq!(
        old.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        7
    );
    let no_confirmation = command(&[
        "--json",
        "upgrade",
        "--backup-dir",
        backup.to_str().unwrap(),
    ]);
    assert!(!no_confirmation.status.success());
    let refused: serde_json::Value = serde_json::from_slice(&no_confirmation.stdout).unwrap();
    assert!(refused["error"]["message"]
        .as_str()
        .unwrap()
        .contains("fence"));
    assert!(refused.get("status").is_none());
    assert!(!backup.exists());
    drop(old);
    let upgraded = command(&[
        "--json",
        "upgrade",
        "--backup-dir",
        backup.to_str().unwrap(),
        "--confirm-writer-fence",
    ]);
    assert!(
        upgraded.status.success(),
        "{}",
        String::from_utf8_lossy(&upgraded.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&upgraded.stdout).unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(result["backup"], "verified");
    assert!(result["instruction"]
        .as_str()
        .unwrap()
        .contains("writer fence"));
    assert_eq!(
        std::fs::metadata(&backup).unwrap().permissions().mode() & 0o077,
        0
    );
    assert_eq!(
        std::fs::metadata(backup.join("another.db"))
            .unwrap()
            .permissions()
            .mode()
            & 0o077,
        0
    );
    assert_eq!(
        std::fs::metadata(backup.join("backup.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o077,
        0
    );
    let copy = Connection::open(backup.join("another.db")).unwrap();
    assert_eq!(
        copy.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        7
    );
    assert_eq!(
        copy.query_row("SELECT display_name FROM nodes", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "committed-wal"
    );
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(backup.join("backup.json")).unwrap()).unwrap();
    assert_eq!(meta["schema_version"], 7);
    assert_eq!(meta["database_sha256"].as_str().unwrap().len(), 64);
    let migrated = Connection::open(&db).unwrap();
    assert_eq!(
        migrated
            .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        10
    );
    assert!(command(&["status"]).status.success());
}

/// The upgrade every existing home takes: a real v8 schema, with sessions and holds in
/// it, becomes v9 only through the fenced offline upgrade, keeps every historical event
/// byte, and gains exactly one admission key.
#[cfg(unix)]
#[test]
fn v8_home_upgrades_to_v10_keeping_history_and_gaining_one_admission_key() {
    use rusqlite::Connection;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let backup = dir.path().join("backup");
    let root = dir.path().join("shop");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join(".git/config"), "[core]\n\tbare = false\n").unwrap();
    let command = |args: &[&str]| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_another"));
        cmd.args(args)
            .env("ANOTHER_HOME", &home)
            .env("HOME", dir.path())
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_AGENT_MODE")
            .env_remove("GROK_HOOK_EVENT");
        cmd
    };
    let hook = |event: &str, input: serde_json::Value| -> String {
        let mut child = command(&["hook", "run", event, "--client", "claude"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    };
    assert!(command(&["init"]).output().unwrap().status.success());
    let started = hook(
        "session-start",
        json!({"session_id": "before-upgrade", "cwd": root}),
    );
    let actor = started
        .strip_prefix("Another: ")
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();

    // Take the home back to exactly the v8 schema: v9 added these objects and nothing else.
    let db = Connection::open(home.join("another.db")).unwrap();
    db.execute_batch(
        "DROP TABLE admission_holds; DROP TABLE admissions; DROP TABLE admission_secret; PRAGMA user_version=8;",
    )
    .unwrap();
    let events = |db: &Connection| -> Vec<String> {
        let mut stmt = db
            .prepare("SELECT event_id||':'||hex(payload)||':'||coalesce(sig,'') FROM events ORDER BY seq")
            .unwrap();
        let rows = stmt.query_map([], |r| r.get(0)).unwrap();
        rows.map(Result::unwrap).collect()
    };
    let history = events(&db);
    drop(db);

    // A v8 home is refused by every route until it is upgraded, and is not changed.
    let status = command(&["status"]).output().unwrap();
    assert!(!status.status.success());
    assert!(String::from_utf8_lossy(&status.stderr).contains("offline upgrade"));
    let edit = hook(
        "pre-native-edit",
        json!({"session_id": "before-upgrade", "cwd": root, "tool_use_id": "t1",
            "tool_name": "Edit", "tool_input": {"file_path": root.join("a.rs")}}),
    );
    assert!(edit.contains("\"deny\""), "{edit}");
    let db = Connection::open(home.join("another.db")).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        8
    );
    assert_eq!(events(&db), history);
    drop(db);

    let upgraded = command(&[
        "--json",
        "upgrade",
        "--backup-dir",
        backup.to_str().unwrap(),
        "--confirm-writer-fence",
    ])
    .output()
    .unwrap();
    assert!(
        upgraded.status.success(),
        "{}",
        String::from_utf8_lossy(&upgraded.stderr)
    );
    let saved = Connection::open(backup.join("another.db")).unwrap();
    assert_eq!(
        saved
            .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        8
    );
    assert_eq!(events(&saved), history);

    let db = Connection::open(home.join("another.db")).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        10
    );
    assert_eq!(events(&db), history, "the upgrade rewrote history");
    let (keys, length): (i64, i64) = db
        .query_row(
            "SELECT count(*), max(length(secret)) FROM admission_secret",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((keys, length), (1, 32));
    let admissions: i64 = db
        .query_row("SELECT count(*) FROM admissions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(admissions, 0);

    // The upgraded home opens, and the session from before the upgrade is still itself.
    assert!(command(&["status"]).output().unwrap().status.success());
    let resumed = hook(
        "session-start",
        json!({"session_id": "before-upgrade", "cwd": root}),
    );
    assert!(
        resumed.starts_with(&format!("Another: {actor} ")),
        "{resumed}"
    );

    // A v8 home that is not the v8 schema is not migrated.
    let other = dir.path().join("other-home");
    let other_backup = dir.path().join("other-backup");
    let other_command = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_another"))
            .args(args)
            .env("ANOTHER_HOME", &other)
            .env("HOME", dir.path())
            .env_remove("ANOTHER_AGENT")
            .output()
            .unwrap()
    };
    assert!(other_command(&["init"]).status.success());
    let db = Connection::open(other.join("another.db")).unwrap();
    db.execute_batch(
        "DROP TABLE admission_holds; DROP TABLE admissions; DROP TABLE admission_secret; PRAGMA user_version=8; CREATE TABLE stray(v TEXT);",
    )
    .unwrap();
    drop(db);
    let refused = other_command(&[
        "upgrade",
        "--backup-dir",
        other_backup.to_str().unwrap(),
        "--confirm-writer-fence",
    ]);
    assert!(!refused.status.success());
    let db = Connection::open(other.join("another.db")).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        8
    );
    let stray: i64 = db
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='stray'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stray, 1);
}

#[cfg(unix)]
#[test]
fn failed_migration_keeps_original_and_verified_backup() {
    use rusqlite::Connection;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let backup = dir.path().join("backup");
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_another"))
            .args(args)
            .env("ANOTHER_HOME", &home)
            .env("HOME", dir.path())
            .env_remove("ANOTHER_AGENT")
            .output()
            .unwrap()
    };
    assert!(run(&["init"]).status.success());
    let db = home.join("another.db");
    let old = Connection::open(&db).unwrap();
    old.execute_batch("PRAGMA user_version=7;").unwrap();
    drop(old);
    let out = run(&[
        "--json",
        "upgrade",
        "--backup-dir",
        backup.to_str().unwrap(),
        "--confirm-writer-fence",
    ]);
    assert!(!out.status.success());
    let failed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(failed["error"]["message"].is_string());
    assert!(failed.get("status").is_none());
    assert!(backup.join("backup.json").is_file());
    let current = Connection::open(&db).unwrap();
    assert_eq!(
        current
            .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        7
    );
    assert!(!run(&["status"]).status.success());
}

#[cfg(target_os = "macos")]
#[test]
fn upgrade_commit_write_failure_is_reported_as_uncertain_json() {
    use rusqlite::Connection;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let backup = dir.path().join("backup");
    let init = Command::new(env!("CARGO_BIN_EXE_another"))
        .arg("init")
        .env("ANOTHER_HOME", &home)
        .env("HOME", dir.path())
        .env_remove("ANOTHER_AGENT")
        .output()
        .unwrap();
    assert!(init.status.success());
    let db = home.join("another.db");
    let keeper = Connection::open(&db).unwrap();
    keeper
        .execute_batch("PRAGMA wal_autocheckpoint=0; DROP TABLE admission_holds; DROP TABLE admissions; DROP TABLE admission_secret; DROP TABLE announcements; PRAGMA user_version=7;")
        .unwrap();
    // Grow the WAL through a catalog table: a table the schema does not define would be
    // refused before the commit this test is about.
    for n in 0..256 {
        keeper
            .execute("UPDATE nodes SET display_name = ?1", [format!("pad-{n}")])
            .unwrap();
    }
    let wal_bytes = std::fs::metadata(home.join("another.db-wal"))
        .unwrap()
        .len();
    let limit_kib = wal_bytes.div_ceil(1024);
    assert!(wal_bytes > std::fs::metadata(&db).unwrap().len() * 2);
    let output = Command::new("/bin/bash")
        .arg("-c")
        .arg("trap '' XFSZ; ulimit -f \"$1\" || exit 125; shift; exec \"$@\"")
        .arg("upgrade-fault-test")
        .arg(limit_kib.to_string())
        .arg(env!("CARGO_BIN_EXE_another"))
        .args(["--json", "upgrade", "--backup-dir"])
        .arg(&backup)
        .arg("--confirm-writer-fence")
        .env("ANOTHER_HOME", &home)
        .env("HOME", dir.path())
        .env_remove("ANOTHER_AGENT")
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let message = response["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("upgrade commit outcome uncertain"),
        "{message}"
    );
    assert!(message.contains("keep the writer fence"), "{message}");
    assert!(response.get("status").is_none());
    assert!(backup.join("another.db").is_file());
    assert!(backup.join("backup.json").is_file());
    let saved = Connection::open(backup.join("another.db")).unwrap();
    assert_eq!(
        saved
            .pragma_query_value(None, "integrity_check", |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    assert_eq!(
        saved
            .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        7
    );
    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(backup.join("backup.json")).unwrap()).unwrap();
    assert_eq!(metadata["schema_version"], 7);
    let version: i64 = keeper
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert!(version == 7 || version == 9, "unexpected schema {version}");
    assert_eq!(
        keeper
            .query_row("SELECT display_name FROM nodes", [], |r| r
                .get::<_, String>(0))
            .unwrap(),
        "pad-255"
    );
    eprintln!("commit fault: {message}; source schema after inspection: v{version}");
    // An uncertain result requires inspection; the test never retries or restores the source.
}
