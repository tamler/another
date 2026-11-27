//! `another sweep` and `another pair revoke`, the two remaining CLI commands no test ran.
//! The work they act on is set up through the store in the same home: a hold and a request
//! made an hour ago (a store opened with a clock in the past), and a spoke paired in process.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use another_core::*;

fn run(home: &Path, cwd: &Path, args: &[&str]) -> (bool, String) {
    let out = {
        let mut legacy_fixture_command = Command::new(env!("CARGO_BIN_EXE_another"));
        legacy_fixture_command.arg("--unguarded-repair");
        legacy_fixture_command
    }
    .args(args)
    .current_dir(cwd)
    .env("ANOTHER_HOME", home)
    .env_remove("ANOTHER_AGENT")
    .env_remove("ANOTHER_LOG")
    .stdin(Stdio::null())
    .output()
    .unwrap();
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[test]
fn sweep_reports_and_ends_lapsed_holds_and_overdue_requests() {
    let dir = tempfile::tempdir().unwrap();
    let home: PathBuf = dir.path().join("home");
    let shop = dir.path().join("shop");
    std::fs::create_dir_all(&shop).unwrap();
    {
        // An hour ago: a one-minute hold and a request due in a minute.
        let (clock, _) = Clock::manual(now_ms() - 3_600_000);
        let store = Store::open_with_clock(&home, clock)
            .unwrap()
            .unguarded_repair();
        for name in ["lead", "worker"] {
            store
                .register_agent(RegisterAgent {
                    name: name.into(),
                    program: Program::Other,
                    project: Some(shop.to_string_lossy().into_owned()),
                    model: None,
                    description: None,
                })
                .unwrap();
        }
        store
            .acquire_holds(AcquireHolds {
                agent: "worker@shop".into(),
                project: shop.to_string_lossy().into_owned(),
                paths: vec!["src/a.rs".into()],
                ttl_seconds: Some(60),
                exclusive: true,
                reason: None,
                repo_wide: false,
            })
            .unwrap();
        store
            .send_message(SendMessage {
                from: "lead@shop".into(),
                to: vec!["worker@shop".into()],
                subject: "due".into(),
                body: "b".into(),
                request: Some(NewRequest {
                    due_in_seconds: Some(60),
                }),
                ..Default::default()
            })
            .unwrap();
    }

    let (ok, out) = run(&home, &shop, &["sweep"]);
    assert!(ok, "{out}");
    // The counts, not the whole line: #189 adds resource leases to this sentence
    // ("expired 1 hold(s) and 0 resource lease(s), 1 request(s) went stale").
    assert!(out.contains("expired 1 hold(s)"), "{out}");
    assert!(out.contains("1 request(s) went stale"), "{out}");
    // Swept once: a second sweep finds nothing, in JSON too.
    let (ok, out) = run(&home, &shop, &["--json", "sweep"]);
    assert!(ok, "{out}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["expired_holds"], 0, "{v}");
    assert_eq!(v["stale_requests"], 0, "{v}");
    let (_, holds) = run(&home, &shop, &["--json", "holds"]);
    assert_eq!(holds.trim(), "[]", "{holds}");
}

#[test]
fn pair_revoke_marks_the_peer_revoked_and_unknown_nodes_fail() {
    let dir = tempfile::tempdir().unwrap();
    let hub_home = dir.path().join("hub");
    {
        let hub = Store::open(&hub_home).unwrap();
        hub.set_node_name("hub").unwrap();
        let spoke = Store::open(&dir.path().join("spoke")).unwrap();
        spoke.set_node_name("spoke").unwrap();
        let code = hub
            .create_invite("https://hub.example", &Perm::ALL, 3600)
            .unwrap();
        let (invite, req) = spoke.prepare_join(&code).unwrap();
        let resp = hub.accept_pairing(&req).unwrap();
        spoke.complete_join(&invite, &resp).unwrap();
    }
    let cwd = dir.path();

    let (ok, peers) = run(&hub_home, cwd, &["peers"]);
    assert!(ok, "{peers}");
    assert!(peers.contains("spoke"), "{peers}");
    assert!(!peers.contains("(revoked)"), "{peers}");

    let (ok, out) = run(&hub_home, cwd, &["pair", "revoke", "spoke"]);
    assert!(ok, "{out}");
    assert_eq!(out.trim(), "revoked spoke", "{out}");

    let (_, peers) = run(&hub_home, cwd, &["peers"]);
    assert!(peers.contains("(revoked)"), "{peers}");
    let (_, json) = run(&hub_home, cwd, &["--json", "peers"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(v[0]["revoked_at"].is_string(), "{v}");

    let (ok, out) = run(&hub_home, cwd, &["pair", "revoke", "nobody"]);
    assert!(!ok, "revoking an unknown node succeeded: {out}");
}
