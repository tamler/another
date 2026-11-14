#![cfg(unix)]

use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::process::{Command, Stdio};

#[test]
fn corrupt_home_session_start_reports_identity_error() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::write(home.join("another.db"), b"not sqlite").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["hook", "run", "session-start", "--client", "claude"])
        .env("ANOTHER_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let input = serde_json::json!({"session_id":"corrupt", "cwd":temp.path()});
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("identity unavailable") && text.contains("database"),
        "{text:?}"
    );
}

#[test]
fn missing_home_denies_pre_edit_without_creating_it() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("missing");
    let mut child = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["hook", "run", "pre-native-edit", "--client", "claude"])
        .env("ANOTHER_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let input = serde_json::json!({"session_id":"missing", "cwd":temp.path(),
        "hook_event_name":"PreToolUse", "tool_name":"Edit",
        "tool_input":{"file_path":temp.path().join("file.rs")}});
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let reply: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reply["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(reply["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap()
        .contains("database"));
    assert!(!home.exists());
}

#[test]
fn corrupt_database_denies_pre_edit_without_changing_home() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("another-home");
    let project = temp.path().join("project");
    std::fs::create_dir(&home).unwrap();
    std::fs::create_dir(&project).unwrap();
    let db = home.join("another.db");
    std::fs::write(&db, b"this is not a sqlite database").unwrap();
    let before_bytes = std::fs::read(&db).unwrap();
    let before_db = std::fs::metadata(&db).unwrap();
    let before_home = std::fs::metadata(&home).unwrap();
    let before_names = std::fs::read_dir(&home).unwrap().count();
    let mut child = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["hook", "run", "pre-native-edit", "--client", "claude"])
        .env("ANOTHER_HOME", &home)
        .env_remove("ANOTHER_AGENT")
        .env_remove("ANOTHER_AGENT_MODE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let input = serde_json::json!({"session_id":"corrupt-fixture", "cwd":project,
        "hook_event_name":"PreToolUse", "tool_name":"Edit",
        "tool_input":{"file_path":project.join("file.rs")}});
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let reply: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "expected denial, got {:?}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(reply["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(reply["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap()
        .contains("database"));
    let after_db = std::fs::metadata(&db).unwrap();
    let after_home = std::fs::metadata(&home).unwrap();
    assert_eq!(before_bytes, std::fs::read(&db).unwrap());
    assert_eq!(
        (
            before_db.ino(),
            before_db.mode(),
            before_db.mtime(),
            before_db.mtime_nsec()
        ),
        (
            after_db.ino(),
            after_db.mode(),
            after_db.mtime(),
            after_db.mtime_nsec()
        )
    );
    assert_eq!(
        (
            before_home.ino(),
            before_home.mode(),
            before_home.mtime(),
            before_home.mtime_nsec(),
            before_names
        ),
        (
            after_home.ino(),
            after_home.mode(),
            after_home.mtime(),
            after_home.mtime_nsec(),
            std::fs::read_dir(&home).unwrap().count()
        )
    );
}

#[test]
fn malformed_schema_is_denied_without_repair() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    std::fs::create_dir(&home).unwrap();
    std::fs::create_dir(&project).unwrap();
    let db = home.join("another.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE retained(value TEXT); INSERT INTO retained VALUES ('preserve'); PRAGMA user_version=8; PRAGMA writable_schema=ON; UPDATE sqlite_schema SET sql='CREATE TABLE retained(' WHERE name='retained'; PRAGMA writable_schema=OFF;").unwrap();
    }
    let bytes = std::fs::read(&db).unwrap();
    let meta = std::fs::metadata(&db).unwrap();
    let home_meta = std::fs::metadata(&home).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["hook", "run", "pre-native-edit", "--client", "claude"])
        .env("ANOTHER_HOME", &home)
        .env_remove("ANOTHER_AGENT")
        .env_remove("ANOTHER_AGENT_MODE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let input = serde_json::json!({"session_id":"malformed", "cwd":project,
        "hook_event_name":"PreToolUse", "tool_name":"Edit",
        "tool_input":{"file_path":project.join("file.rs")}});
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let reply: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reply["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(reply["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap()
        .contains("malformed"));
    let after = std::fs::metadata(&db).unwrap();
    let after_home = std::fs::metadata(&home).unwrap();
    assert_eq!(bytes, std::fs::read(&db).unwrap());
    assert_eq!(
        (meta.ino(), meta.mode(), meta.mtime(), meta.mtime_nsec()),
        (after.ino(), after.mode(), after.mtime(), after.mtime_nsec())
    );
    assert_eq!(
        (
            home_meta.ino(),
            home_meta.mode(),
            home_meta.mtime(),
            home_meta.mtime_nsec()
        ),
        (
            after_home.ino(),
            after_home.mode(),
            after_home.mtime(),
            after_home.mtime_nsec()
        )
    );
    assert_eq!(std::fs::read_dir(&home).unwrap().count(), 1);
}
