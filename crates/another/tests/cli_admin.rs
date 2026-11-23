//! CLI commands no other test drove: `agent show`, `hold renew`, `node name`,
//! `request ... cancel` and `audit`, through the real binary.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::Value;

fn another(home: &Path, args: &[&str]) -> Output {
    {
        let mut legacy_fixture_command = Command::new(env!("CARGO_BIN_EXE_another"));
        legacy_fixture_command.arg("--unguarded-repair");
        legacy_fixture_command
    }
    .args(args)
    .env("ANOTHER_HOME", home)
    .env_remove("ANOTHER_AGENT")
    .env_remove("ANOTHER_LOG")
    .stdin(Stdio::null())
    .output()
    .expect("run another")
}

fn json(home: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend_from_slice(args);
    let out = another(home, &all);
    assert!(
        out.status.success(),
        "another {args:?} failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("json output")
}

/// Exit code and stderr of a command expected to fail.
fn fails(home: &Path, args: &[&str]) -> (i32, String) {
    let out = another(home, args);
    assert!(!out.status.success(), "another {args:?} should fail");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn setup() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("h");
    json(
        &home,
        &["agent", "register", "alice", "--program", "claude"],
    );
    json(&home, &["agent", "register", "bob", "--program", "codex"]);
    (dir, home)
}

#[test]
fn agent_show_describes_one_agent_and_names_an_unknown_one() {
    let (_dir, home) = setup();
    let alice = json(&home, &["agent", "show", "alice"]);
    assert_eq!(alice["name"], "alice");
    assert_eq!(alice["program"], "claude");
    assert_eq!(alice["status"], "active");
    assert_eq!(alice["agent_id"].as_str().unwrap().len(), 26);

    let (code, err) = fails(&home, &["agent", "show", "nobody"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("unknown agent \"nobody\""), "{err}");
}

#[test]
fn hold_renew_renews_own_holds_and_refuses_anothers() {
    let (dir, home) = setup();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let held = json(
        &home,
        &[
            "hold",
            "acquire",
            "--agent",
            "alice",
            "--project",
            repo.to_str().unwrap(),
            "src/a.rs",
        ],
    );
    let id = held["granted"][0]["hold_id"].as_str().unwrap().to_string();
    let before = held["granted"][0]["expires_at"]
        .as_str()
        .unwrap()
        .to_string();
    std::thread::sleep(std::time::Duration::from_millis(20));

    let renewed = json(&home, &["hold", "renew", "--agent", "alice", &id]);
    assert_eq!(renewed[0]["hold_id"], id.as_str());
    assert!(renewed[0]["expires_at"].as_str().unwrap() > before.as_str());

    // Another agent cannot renew it: an error naming the hold, not a silent no-op.
    let (code, err) = fails(&home, &["hold", "renew", "--agent", "bob", &id]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("no live hold"), "{err}");

    // Renewal always names exact subjects, including for an actor with no holds.
    for agent in ["alice", "bob"] {
        let (code, err) = fails(&home, &["hold", "renew", "--agent", agent]);
        assert_eq!(code, 1);
        assert!(err.contains("renew must name exact hold ids"), "{err}");
    }
}

#[test]
fn node_name_prints_renames_and_validates() {
    let (_dir, home) = setup();
    let out = another(&home, &["node", "name"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("local"));

    let renamed = json(&home, &["node", "name", "hub-one"]);
    assert_eq!(renamed["display_name"], "hub-one");
    assert_eq!(json(&home, &["node", "show"])["display_name"], "hub-one");

    let (code, err) = fails(&home, &["node", "name", "bad name!"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("invalid node name"), "{err}");
    assert_eq!(json(&home, &["node", "show"])["display_name"], "hub-one");
}

#[test]
fn only_the_creator_cancels_a_request_and_only_once() {
    let (_dir, home) = setup();
    let sent = json(
        &home,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--subject",
            "Review",
            "--body",
            "please",
            "--due",
            "600",
        ],
    );
    let rid = sent["request_id"].as_str().unwrap().to_string();

    let (code, err) = fails(&home, &["request", "--agent", "bob", "cancel", &rid]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("only the creator"), "{err}");

    let cancelled = json(&home, &["request", "--agent", "alice", "cancel", &rid]);
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(cancelled["created_by"], "alice");

    // A second cancel is a conflict (exit 3), and the owner can no longer claim it.
    let (code, err) = fails(&home, &["request", "--agent", "alice", "cancel", &rid]);
    assert_eq!(code, 3, "{err}");
    assert!(err.contains("already cancelled"), "{err}");
    let (code, _) = fails(&home, &["request", "--agent", "bob", "claim", &rid]);
    assert_ne!(code, 0);
}

#[test]
fn audit_is_empty_without_remote_calls() {
    let (_dir, home) = setup();
    assert_eq!(json(&home, &["audit"]), serde_json::json!([]));
    assert!(another(&home, &["audit", "--limit", "5"]).status.success());
}
