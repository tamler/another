use std::io::Write;
use std::process::{Command, Output, Stdio};

use rusqlite::{params, Connection};
use serde_json::{json, Value};

#[test]
fn missing_binding_for_exact_session_actor_never_creates_a_new_identity() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("shop");
    let home = dir.path().join("home");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(root.join(".git/config"), "[core]\n\tbare = false\n").unwrap();

    let command = |args: &[&str]| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_another"));
        cmd.args(args)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("ANOTHER_HOME", home.join(".another"))
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_AGENT_MODE")
            .env_remove("GROK_HOOK_EVENT")
            .env_remove("HERDR_PANE_ID")
            .env_remove("TMUX_PANE")
            .env_remove("CMUX_SURFACE_ID");
        cmd
    };
    let hook = |event: &str, input: Value, actor: Option<&str>| -> Output {
        let mut cmd = command(&["hook", "run", event, "--client", "claude"]);
        if let Some(actor) = actor {
            cmd.env("ANOTHER_AGENT", actor)
                .env("ANOTHER_AGENT_MODE", "session");
        }
        let mut child = cmd
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
    let start = |session: &str| {
        let out = hook(
            "session-start",
            json!({"session_id":session, "cwd":root}),
            None,
        );
        assert!(out.status.success(), "start: {:?}", out);
        let text = String::from_utf8(out.stdout).unwrap();
        text.strip_prefix("Another: ")
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned()
    };
    // Hooks never create a home: it exists before any session starts.
    assert!(command(&["init"]).output().unwrap().status.success());
    let actor = start("original-session");
    let sender = start("sender-session");
    let edit = |path: &str| {
        json!({"session_id":"original-session", "cwd":root,
        "hook_event_name":"PreToolUse", "tool_name":"Edit", "tool_input":{"file_path":root.join(path)}})
    };
    let initial = hook("pre-edit", edit("src/original.rs"), None);
    assert!(initial.status.success());
    assert_eq!(initial.stdout, b"");
    let sent = command(&[
        "send",
        "--from",
        &sender,
        "--to",
        &actor,
        "--subject",
        "retained mail",
        "--body",
        "fixture",
    ])
    .output()
    .unwrap();
    assert!(sent.status.success(), "send: {:?}", sent);

    let db = Connection::open(home.join(".another/another.db")).unwrap();
    let actor_id: String = db
        .query_row(
            "SELECT agent_id FROM agents WHERE address = ?1",
            [&actor],
            |r| r.get(0),
        )
        .unwrap();
    let counts = |db: &Connection| -> (i64, i64, i64, i64) {
        (
            db.query_row("SELECT count(*) FROM agents", [], |r| r.get(0))
                .unwrap(),
            db.query_row(
                "SELECT count(*) FROM events WHERE kind='session.binding.v1'",
                [],
                |r| r.get(0),
            )
            .unwrap(),
            db.query_row(
                "SELECT count(*) FROM holds WHERE agent_id=?1 AND released_at IS NULL",
                [&actor_id],
                |r| r.get(0),
            )
            .unwrap(),
            db.query_row(
                "SELECT count(*) FROM deliveries WHERE agent_id=?1",
                [&actor_id],
                |r| r.get(0),
            )
            .unwrap(),
        )
    };
    assert_eq!(
        db.execute(
            "DELETE FROM events WHERE kind='session.binding.v1' AND actor=?1",
            params![actor]
        )
        .unwrap(),
        1
    );
    let before = counts(&db);
    assert_eq!(
        (before.2, before.3),
        (1, 1),
        "fixture must preserve hold and mail"
    );

    let resumed = hook(
        "session-start",
        json!({"session_id":"original-session", "cwd":root}),
        Some(&actor),
    );
    let after_start = counts(&db);
    let attempted_edit = hook("pre-edit", edit("src/other.rs"), Some(&actor));
    let after_edit = counts(&db);
    let output = String::from_utf8_lossy(&attempted_edit.stdout);
    assert!(resumed.status.success() && attempted_edit.status.success() && after_start == before && after_edit == before
        && serde_json::from_str::<Value>(&output).ok().is_some_and(|v| v["hookSpecificOutput"]["permissionDecision"] == "deny"),
        "missing binding must not create a new actor/binding and pre-edit must deny: before={before:?}, after_start={after_start:?}, after_edit={after_edit:?}, start_stdout={:?}, start_stderr={:?}, edit_stdout={:?}, edit_stderr={:?}",
        String::from_utf8_lossy(&resumed.stdout), String::from_utf8_lossy(&resumed.stderr), output, String::from_utf8_lossy(&attempted_edit.stderr));
}
