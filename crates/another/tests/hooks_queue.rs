//! SPEC-B7 supersedes the D96 routine queue digest policy.
use another::hooks::{self, Client, HookEnv, HookEvent, HookInput};
use another_core::{Clock, Store, SubjectKind};
use serde_json::json;
use std::sync::atomic::Ordering;

#[test]
fn quiet_events_and_idle_reminder_do_not_renew_files() {
    let dir = tempfile::tempdir().unwrap();
    let (clock, now) = Clock::manual(1_800_000_000_000);
    let store = Store::open_with_clock(&dir.path().join("state"), clock).unwrap();
    let env = HookEnv {
        home: dir.path().join("user"),
        another_home: dir.path().join("state"),
        exe: std::env::current_exe().unwrap(),
    };
    let run = |event, tool, call| {
        hooks::run(
            &store,
            &env,
            event,
            Client::Claude,
            HookInput::parse(
                &json!({
                    "cwd":dir.path(), "session_id":"root", "tool_name":tool,
                    "tool_use_id":call, "tool_input":{"file_path":"A"}
                })
                .to_string(),
            ),
        )
        .unwrap()
    };
    assert!(run(HookEvent::SessionStart, "Read", "start").is_empty());
    assert!(run(HookEvent::PreEdit, "Write", "write").is_empty());
    let before = store
        .right_leases()
        .unwrap()
        .into_iter()
        .find(|l| l.subject_kind == SubjectKind::File)
        .unwrap();
    now.fetch_add(120_001, Ordering::SeqCst);
    let reminder = run(HookEvent::Prompt, "Read", "prompt");
    assert!(!reminder.is_empty());
    assert!(!reminder.contains("Another queue:"));
    assert!(!reminder.contains("Subjects,"));
    assert!(run(HookEvent::Prompt, "Read", "again").is_empty());
    let after = store
        .right_leases()
        .unwrap()
        .into_iter()
        .find(|l| l.lease_id == before.lease_id)
        .unwrap();
    assert_eq!(before.expires_at, after.expires_at);
    assert_eq!(before.generation, after.generation);
}

#[test]
fn status_does_not_initialize_a_missing_home() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("absent");
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_another"))
        .arg("status")
        .env("ANOTHER_HOME", &home)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(!home.exists());
}

#[test]
fn published_policy_uses_shared_duration_and_authority() {
    for text in [
        include_str!("../../../docs/HOOKS.md"),
        include_str!("../../../docs/HOLDS.md"),
        include_str!("../../../docs/AGENT-CONTRACT.md"),
    ] {
        assert!(text.contains("120"));
        assert!(text.contains("30"));
        assert!(text.contains("Cursor"));
        assert!(!text.contains("default 30 minutes"));
    }
}
