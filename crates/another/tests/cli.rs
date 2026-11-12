//! Drive the `another` binary the way hooks and scripts do.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::Value;

fn another(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_another"))
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

#[test]
fn init_creates_home_with_node() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("h");
    let status = json(&home, &["init"]);
    assert_eq!(status["node"]["node_id"].as_str().unwrap().len(), 26);
    assert!(home.join("another.db").exists());
    let events = json(&home, &["events"]);
    assert_eq!(events[0]["kind"], "node.created");
}

#[test]
fn mail_requests_and_holds_from_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("h");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let repo_s = repo.to_str().unwrap();

    json(
        &home,
        &["agent", "register", "alice", "--program", "claude"],
    );
    json(&home, &["agent", "register", "bob", "--program", "grok"]);

    let sent = json(
        &home,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--subject",
            "Review PR",
            "--body",
            "please",
            "--due",
            "3600",
        ],
    );
    let mid = sent["message_id"].as_str().unwrap().to_string();
    let rid = sent["request_id"].as_str().unwrap().to_string();

    let inbox = json(&home, &["inbox", "--agent", "bob", "--unread"]);
    assert_eq!(inbox["unread"], 1);
    assert_eq!(inbox["requests"][0]["request_id"], rid.as_str());

    json(&home, &["ack", "--agent", "bob", &mid]);
    let reply = json(&home, &["reply", &mid, "--from", "bob", "--body", "lgtm"]);
    assert_eq!(reply["thread_id"], sent["thread_id"]);
    let thread = json(&home, &["thread", sent["thread_id"].as_str().unwrap()]);
    assert_eq!(thread["messages"].as_array().unwrap().len(), 2);

    let claimed = json(&home, &["request", "claim", &rid, "--agent", "bob"]);
    assert_eq!(claimed["owner"], "bob");
    let done = json(
        &home,
        &[
            "request", "complete", &rid, "--agent", "bob", "--note", "merged",
        ],
    );
    assert_eq!(done["status"], "done");
    let listed = json(&home, &["requests", "--status", "done"]);
    assert_eq!(listed.as_array().unwrap().len(), 1);

    let held = json(
        &home,
        &[
            "hold",
            "acquire",
            "--agent",
            "alice",
            "--project",
            repo_s,
            "src/**",
            "--ttl",
            "600",
        ],
    );
    assert_eq!(held["granted"].as_array().unwrap().len(), 1);

    // A conflicting acquire exits 3 and grants nothing.
    let clash = another(
        &home,
        &[
            "--json",
            "hold",
            "acquire",
            "--agent",
            "bob",
            "--project",
            repo_s,
            "src/lib.rs",
        ],
    );
    assert_eq!(clash.status.code(), Some(3));
    let clash: Value = serde_json::from_slice(&clash.stdout).unwrap();
    assert_eq!(clash["conflicts"][0]["held"]["agent"], "alice");

    let hb = json(&home, &["heartbeat", "--agent", "alice", "--end"]);
    assert_eq!(hb["released_holds"].as_array().unwrap().len(), 1);
    let check = another(
        &home,
        &[
            "hold",
            "check",
            "--agent",
            "bob",
            "--project",
            repo_s,
            "src/lib.rs",
        ],
    );
    assert!(check.status.success());

    // Human output works too.
    let text = another(&home, &["inbox", "--agent", "alice"]);
    assert!(text.status.success());
    assert!(String::from_utf8_lossy(&text.stdout).contains("Done: Review PR"));
}

/// `another inbox` is not an ack, `another ack` only records the acting agent's receipt, and
/// `--read-only` marks read without acknowledging.
#[test]
fn inbox_fetch_is_not_an_ack_and_read_only_does_not_ack() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("h");
    for (name, program) in [("alice", "claude"), ("bob", "codex"), ("carol", "grok")] {
        json(&home, &["agent", "register", name, "--program", program]);
    }
    let sent = json(
        &home,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob,carol",
            "--subject",
            "Confirm receipt",
            "--body",
            "please ack",
            "--ack",
        ],
    );
    let mid = sent["message_id"].as_str().unwrap().to_string();

    // Fetching twice (and with --unread) leaves the counters and the delivery untouched.
    let first = json(&home, &["inbox", "--agent", "bob"]);
    assert_eq!(
        (first["unread"].as_u64(), first["awaiting_ack"].as_u64()),
        (Some(1), Some(1))
    );
    let second = json(&home, &["inbox", "--agent", "bob"]);
    assert_eq!(second["unread"], first["unread"]);
    assert_eq!(second["awaiting_ack"], first["awaiting_ack"]);
    assert_eq!(second["messages"][0]["message_id"], mid.as_str());
    assert!(second["messages"][0]["read_at"].is_null());
    assert!(second["messages"][0]["acked_at"].is_null());
    let unread = json(&home, &["inbox", "--agent", "bob", "--unread"]);
    assert_eq!(unread["messages"].as_array().unwrap().len(), 1);
    assert_eq!(json(&home, &["inbox", "--agent", "bob"])["unread"], 1);

    // --read-only: read, not acknowledged.
    let read = json(&home, &["ack", "--agent", "bob", "--read-only", &mid]);
    assert_eq!(read["updated"].as_array().unwrap().len(), 1);
    let bob = json(&home, &["inbox", "--agent", "bob"]);
    assert_eq!(
        (bob["unread"].as_u64(), bob["awaiting_ack"].as_u64()),
        (Some(0), Some(1))
    );
    assert!(bob["messages"][0]["read_at"].is_string());
    assert!(bob["messages"][0]["acked_at"].is_null());

    // Bob's actions never touched Carol's delivery.
    let carol = json(&home, &["inbox", "--agent", "carol"]);
    assert_eq!(
        (carol["unread"].as_u64(), carol["awaiting_ack"].as_u64()),
        (Some(1), Some(1))
    );
    assert!(carol["messages"][0]["read_at"].is_null());
    assert!(carol["messages"][0]["acked_at"].is_null());

    // A real ack records Bob's receipt only.
    let acked = json(&home, &["ack", "--agent", "bob", &mid]);
    assert_eq!(acked["updated"].as_array().unwrap().len(), 1);
    let bob = json(&home, &["inbox", "--agent", "bob"]);
    assert_eq!(bob["awaiting_ack"], 0);
    assert!(bob["messages"][0]["acked_at"].is_string());
    let carol = json(&home, &["inbox", "--agent", "carol"]);
    assert_eq!(
        (carol["unread"].as_u64(), carol["awaiting_ack"].as_u64()),
        (Some(1), Some(1))
    );
    assert!(carol["messages"][0]["acked_at"].is_null());

    // Human output names the counts; Carol's read-only call is her own.
    let text = another(&home, &["ack", "--agent", "carol", "--read-only", &mid]);
    assert!(text.status.success());
    assert!(String::from_utf8_lossy(&text.stdout).contains("1 updated, 0 unchanged"));
    let carol = json(&home, &["inbox", "--agent", "carol"]);
    assert_eq!(
        (carol["unread"].as_u64(), carol["awaiting_ack"].as_u64()),
        (Some(0), Some(1))
    );
}

#[test]
fn errors_are_structured_in_json_mode() {
    let dir = tempfile::tempdir().unwrap();
    let out = another(dir.path(), &["--json", "inbox", "--agent", "nobody"]);
    assert!(!out.status.success());
    let body: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(body["error"]["code"], "not_found");
}

/// BH-5: a duration whose unit multiplication overflows is an error, not a panic.
#[test]
fn absurd_durations_are_errors_not_panics() {
    let dir = tempfile::tempdir().unwrap();
    json(
        dir.path(),
        &["agent", "register", "alice", "--program", "claude"],
    );
    for (expires, message) in [
        ("100000000000000000d", "too large"),
        ("9223372036854775807", "too far in the future"),
    ] {
        let out = another(
            dir.path(),
            &["token", "create", "--agent", "alice", "--expires", expires],
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{expires}: {stderr}");
        assert!(!stderr.contains("panicked"), "{expires}: {stderr}");
        assert!(stderr.contains(message), "{expires}: {stderr}");
    }
}

#[test]
fn serve_refuses_non_loopback_binds() {
    let dir = tempfile::tempdir().unwrap();
    let out = another(dir.path(), &["serve", "--bind", "0.0.0.0:7466"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("loopback"));
}
