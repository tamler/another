#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use rusqlite::Connection;

fn hook(home: &Path, root: &Path, event: &str, input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["hook", "run", event, "--client", "claude"])
        .current_dir(root)
        .env("ANOTHER_HOME", home)
        .env_remove("ANOTHER_AGENT")
        .env_remove("ANOTHER_AGENT_MODE")
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
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn non_utf8_project_root_is_denied_without_creating_identity() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let root = dir.path().join(OsString::from_vec(b"repo-\xff".to_vec()));
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let init = Command::new(env!("CARGO_BIN_EXE_another"))
        .arg("init")
        .env("ANOTHER_HOME", &home)
        .output()
        .unwrap();
    assert!(init.status.success());
    let db = Connection::open(home.join("another.db")).unwrap();
    let snapshot = || -> (i64, i64, i64) {
        (
            db.query_row("SELECT count(*) FROM projects", [], |r| r.get(0))
                .unwrap(),
            db.query_row("SELECT count(*) FROM agents", [], |r| r.get(0))
                .unwrap(),
            db.query_row("SELECT count(*) FROM events", [], |r| r.get(0))
                .unwrap(),
        )
    };
    let before = snapshot();
    let start = hook(
        &home,
        &root,
        "session-start",
        r#"{"session_id":"test-session"}"#,
    );
    assert!(start.status.success());
    let notice = String::from_utf8(start.stdout).unwrap();
    assert!(
        notice.contains("project path") && notice.contains("UTF-8"),
        "{notice:?}"
    );
    assert!(!notice.contains("test-session"));
    assert_eq!(snapshot(), before);
    let edit = hook(
        &home,
        &root,
        "pre-native-edit",
        r#"{"session_id":"test-session","tool_name":"Edit","tool_input":{"file_path":"a.rs"}}"#,
    );
    assert!(edit.status.success());
    let reply: serde_json::Value = serde_json::from_slice(&edit.stdout).unwrap();
    assert_eq!(reply["hookSpecificOutput"]["permissionDecision"], "deny");
    let reason = reply["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(
        reason.contains("project path") && reason.contains("UTF-8"),
        "{reason}"
    );
    assert!(!reason.contains("test-session"));
    assert_eq!(snapshot(), before);
}
