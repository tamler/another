use std::io::Write;
use std::process::{Command, Stdio};

use rusqlite::Connection;

#[test]
fn new_home_preserves_v8_schema() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_another"))
        .arg("init")
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("ANOTHER_HOME", home.path().join(".another"))
        .output()
        .unwrap();
    assert!(output.status.success(), "init: {output:?}");
    let db = Connection::open(home.path().join(".another/another.db")).unwrap();
    let (sqlite_version, sqlite_source): (String, String) = db
        .query_row("SELECT sqlite_version(), sqlite_source_id()", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    eprintln!("SQLite {sqlite_version} source {sqlite_source}");
    assert_eq!(sqlite_version, "3.51.3");
    db.execute_batch("CREATE TABLE executable_sqlite_probe(version TEXT NOT NULL, source TEXT NOT NULL); CREATE TRIGGER executable_sqlite_probe_trigger AFTER INSERT ON events BEGIN INSERT INTO executable_sqlite_probe VALUES (sqlite_version(),sqlite_source_id()); END;").unwrap();
    let probe = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["agent", "register", "fixture", "--program", "claude"])
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("ANOTHER_HOME", home.path().join(".another"))
        .output()
        .unwrap();
    assert!(probe.status.success(), "executable probe: {probe:?}");
    let executable_engine: (String, String) = db
        .query_row(
            "SELECT version,source FROM executable_sqlite_probe",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    eprintln!(
        "Another executable SQLite {} source {}",
        executable_engine.0, executable_engine.1
    );
    assert_eq!(executable_engine, (sqlite_version, sqlite_source));
    let version: i64 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(
        version, 8,
        "identity bindings must not require a schema migration"
    );
}

#[test]
fn real_hooks_reject_a_corrupted_signed_binding() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    let home = dir.path().join("home");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(root.join(".git/config"), "[core]\n bare = false\n").unwrap();
    let hook = |event: &str, input: serde_json::Value| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_another"))
            .args(["hook", "run", event, "--client", "claude"])
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("ANOTHER_HOME", home.join(".another"))
            .env_remove("ANOTHER_AGENT")
            .env_remove("GROK_HOOK_EVENT")
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
        child.wait_with_output().unwrap()
    };
    let start = hook(
        "session-start",
        serde_json::json!({"session_id":"private-fixture-session", "cwd":root}),
    );
    assert!(start.status.success());
    assert!(String::from_utf8_lossy(&start.stdout).starts_with("Another: "));
    let db = Connection::open(home.join(".another/another.db")).unwrap();
    let (rows, raw): (i64, i64) = db.query_row("SELECT count(*),sum(instr(payload,'private-fixture-session') > 0) FROM events WHERE kind='session.binding.v1'", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_eq!((rows, raw), (1, 0));
    db.execute(
        "UPDATE events SET ts='invalid' WHERE kind='session.binding.v1'",
        [],
    )
    .unwrap();
    let edit = hook(
        "pre-edit",
        serde_json::json!({"session_id":"private-fixture-session", "cwd":root, "hook_event_name":"PreToolUse", "tool_name":"Edit", "tool_input":{"file_path":root.join("src/a.rs")}}),
    );
    let reply: serde_json::Value = serde_json::from_slice(&edit.stdout).unwrap();
    assert_eq!(reply["hookSpecificOutput"]["permissionDecision"], "deny");
    let agents: i64 = db
        .query_row("SELECT count(*) FROM agents", [], |r| r.get(0))
        .unwrap();
    let holds: i64 = db
        .query_row("SELECT count(*) FROM holds", [], |r| r.get(0))
        .unwrap();
    assert_eq!((agents, holds), (1, 0));
}

#[test]
fn bundled_sqlite_concurrent_wal_and_checkpoints_preserve_integrity() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("wal.db");
    let db = Connection::open(&db_path).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE records(worker INTEGER, n INTEGER, PRIMARY KEY(worker,n));").unwrap();
    let workers: Vec<_> = (0..2)
        .map(|worker| {
            let path = db_path.clone();
            std::thread::spawn(move || {
                let mut c = Connection::open(path).unwrap();
                c.busy_timeout(std::time::Duration::from_secs(5)).unwrap();
                for n in 0..100 {
                    let tx = c.transaction().unwrap();
                    tx.execute("INSERT INTO records VALUES (?1,?2)", (worker, n))
                        .unwrap();
                    tx.commit().unwrap();
                    if n % 10 == 0 {
                        c.execute_batch("PRAGMA wal_checkpoint(PASSIVE);").unwrap();
                    }
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(
        db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM records", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        200
    );
}

#[test]
fn existing_v8_home_keeps_schema_and_historical_event_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let root = dir.path().join("repo");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join(".git/config"), "[core]\n bare = false\n").unwrap();
    let command = |args: &[&str]| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_another"));
        cmd.args(args)
            .env("HOME", dir.path())
            .env("USERPROFILE", dir.path())
            .env("ANOTHER_HOME", &home)
            .env_remove("ANOTHER_AGENT");
        cmd
    };
    assert!(command(&["init"]).output().unwrap().status.success());
    assert!(
        command(&["agent", "register", "legacy", "--program", "claude"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let db = Connection::open(home.join("another.db")).unwrap();
    let snapshot = |sql: &str| -> Vec<String> {
        let mut stmt = db.prepare(sql).unwrap();
        let result = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<String>, _>>()
            .unwrap();
        result
    };
    let schema = snapshot(
        "SELECT type||':'||name||':'||coalesce(sql,'') FROM sqlite_master ORDER BY type,name",
    );
    let events = snapshot(
        "SELECT event_id||':'||hex(payload)||':'||coalesce(sig,'') FROM events ORDER BY seq",
    );
    let mut child = command(&["hook", "run", "session-start", "--client", "claude"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            serde_json::json!({"cwd":root,"session_id":"new-session"})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success() && String::from_utf8_lossy(&out.stdout).starts_with("Another: "),
        "{out:?}"
    );
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        8
    );
    assert_eq!(
        snapshot(
            "SELECT type||':'||name||':'||coalesce(sql,'') FROM sqlite_master ORDER BY type,name"
        ),
        schema
    );
    let after = snapshot(
        "SELECT event_id||':'||hex(payload)||':'||coalesce(sig,'') FROM events ORDER BY seq",
    );
    assert_eq!(&after[..events.len()], events.as_slice());
    assert!(after.len() > events.len());
}
