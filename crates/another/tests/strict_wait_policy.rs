//! Strict admission waits must use its conservative conflict policy through release.
use std::io::Write;
use std::process::{Command, Output, Stdio};

use serde_json::{json, Value};

#[test]
fn strict_wait_stays_until_every_controlled_class_blocker_ends() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let root = temp.path().join("project");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), b"before").unwrap();
    let root = std::fs::canonicalize(root).unwrap();
    let run = |args: &[&str], input: Option<Value>| -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_another"))
            .args(args)
            .env("ANOTHER_HOME", &home)
            .env("HOME", temp.path())
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_AGENT_MODE")
            .env_remove("GROK_HOOK_EVENT")
            .env_remove("HERDR_PANE_ID")
            .env_remove("TMUX_PANE")
            .env_remove("CMUX_SURFACE_ID")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(input) = input {
            writeln!(child.stdin.take().unwrap(), "{input}").unwrap();
        } else {
            drop(child.stdin.take());
        }
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    };
    run(&["init"], None);
    for (agent, pattern) in [("holder-a", "src/[AB].rs"), ("holder-b", "src/[AC].rs")] {
        run(
            &[
                "agent",
                "register",
                agent,
                "--program",
                "codex",
                "--project",
                root.to_str().unwrap(),
            ],
            None,
        );
        run(
            &[
                "hold",
                "acquire",
                "-a",
                agent,
                "--project",
                root.to_str().unwrap(),
                "--shared",
                pattern,
            ],
            None,
        );
    }
    run(
        &["hook", "run", "session-start", "--client", "claude"],
        Some(json!({"cwd":root,"session_id":"strict-wait-test"})),
    );
    let admission = |call: &str| {
        run(
            &["hook", "run", "pre-controlled-edit", "--client", "claude"],
            Some(json!({
                "cwd":root,"session_id":"strict-wait-test","tool_use_id":call,
                "tool_name":"mcp__another__controlled_edit",
                "tool_input":{"project":root,"path":"src/a.rs","expected_sha256":another_core::controlled_sha256(b"before"),"replacement":"after"}
            })),
        )
    };
    let first = admission("call-1");
    assert!(String::from_utf8_lossy(&first.stdout).contains("deny"));
    let db = rusqlite::Connection::open(home.join("another.db")).unwrap();
    let blockers: String = db
        .query_row("SELECT blockers FROM hold_waits", [], |r| r.get(0))
        .unwrap();
    let blockers = serde_json::from_str::<Vec<String>>(&blockers).unwrap();
    assert_eq!(blockers.len(), 2);
    assert!(blockers.windows(2).all(|pair| pair[0] < pair[1]));
    let pings = || {
        db.query_row(
            "SELECT count(*) FROM messages WHERE subject LIKE 'Hold free:%'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
    };
    let waits = || {
        db.query_row("SELECT count(*) FROM hold_waits", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap()
    };
    run(&["hold", "release", "-a", "holder-a"], None);
    assert_eq!(
        pings(),
        0,
        "holder-b still blocks strict editing; no Hold free ping is valid"
    );
    assert_eq!(
        waits(),
        1,
        "keep the durable strict wait while holder-b remains"
    );
    let second = admission("call-2");
    let denial = String::from_utf8_lossy(&second.stdout);
    assert!(
        denial.contains("deny") && denial.contains("holder-b"),
        "{denial}"
    );
    run(&["hold", "release", "-a", "holder-b"], None);
    assert_eq!(pings(), 1);
    assert_eq!(waits(), 0);
    run(&["hold", "release", "-a", "holder-b"], None);
    assert_eq!(pings(), 1, "only one availability notice");
    assert_eq!(std::fs::read(root.join("src/a.rs")).unwrap(), b"before");
}
