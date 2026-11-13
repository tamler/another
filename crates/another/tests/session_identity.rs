use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::json;

#[test]
fn missing_identity_denies_and_lifecycle_does_not_persist_raw_id() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("another-home");
    let root = dir.path().join("work");
    std::fs::create_dir_all(&root).unwrap();
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
    let base_input = json!({"cwd": root, "session_id": raw, "tool_name":"Edit", "tool_input":{"file_path":"test.rs"}});
    let mut equal_alias = base_input.clone();
    equal_alias["sessionId"] = json!(raw);
    equal_alias["conversation_id"] = json!(raw);
    let allowed = run(
        &["hook", "run", "pre-edit", "--client", "claude"],
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
            &["hook", "run", "pre-edit", "--client", "claude"],
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
    let expiry = first_holds[0]["expires_at"].clone();
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
    assert_eq!(after_holds.as_array().unwrap().len(), 1);
    assert_eq!(after_holds[0]["expires_at"], expiry);
    let targets = run(&["--json", "wake", "list"], None);
    assert!(targets.status.success());
    let targets: serde_json::Value = serde_json::from_slice(&targets.stdout).unwrap();
    assert_eq!(targets[0]["target"], "w1:p9");
    let input = json!({"cwd":root,"tool_name":"Edit","tool_input":{"file_path":"test.rs"}});
    let out = run(
        &["hook", "run", "pre-edit", "--client", "claude"],
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
    old.execute_batch("DROP TABLE session_bindings; PRAGMA user_version=8; CREATE TABLE preserved_data(v TEXT); INSERT INTO preserved_data VALUES ('committed-wal');").unwrap();
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
        8
    );
    let no_confirmation = command(&["upgrade", "--backup-dir", backup.to_str().unwrap()]);
    assert!(!no_confirmation.status.success());
    assert!(!backup.exists());
    drop(old);
    let upgraded = command(&[
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
        8
    );
    assert_eq!(
        copy.query_row("SELECT v FROM preserved_data", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "committed-wal"
    );
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(backup.join("backup.json")).unwrap()).unwrap();
    assert_eq!(meta["schema_version"], 8);
    assert_eq!(meta["database_sha256"].as_str().unwrap().len(), 64);
    let migrated = Connection::open(&db).unwrap();
    assert_eq!(
        migrated
            .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        9
    );
    assert!(command(&["status"]).status.success());
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
    old.execute_batch("PRAGMA user_version=8;").unwrap();
    drop(old);
    let out = run(&[
        "upgrade",
        "--backup-dir",
        backup.to_str().unwrap(),
        "--confirm-writer-fence",
    ]);
    assert!(!out.status.success());
    assert!(backup.join("backup.json").is_file());
    let current = Connection::open(&db).unwrap();
    assert_eq!(
        current
            .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        8
    );
    assert!(!run(&["status"]).status.success());
}
