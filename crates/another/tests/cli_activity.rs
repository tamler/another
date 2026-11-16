//! `another activity` as an operator or script runs it: the summary, the `--kinds` filter,
//! each form `--since` takes (a duration, an RFC 3339 time, an event id cursor) and the
//! errors for a kind or since it does not know. No test ran the CLI command.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

struct Fx {
    _dir: tempfile::TempDir,
    home: PathBuf,
    shop: PathBuf,
}

/// Exit status, stdout and stderr.
fn run(home: &Path, cwd: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(args)
        .current_dir(cwd)
        .env("ANOTHER_HOME", home)
        .env_remove("ANOTHER_AGENT")
        .env_remove("ANOTHER_LOG")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

impl Fx {
    /// Two agents in project shop, one message from alice to bob and one hold by alice.
    fn new() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let shop = dir.path().join("shop");
        std::fs::create_dir_all(&shop).unwrap();
        let fx = Fx {
            _dir: dir,
            home,
            shop,
        };
        fx.ok(&["init"]);
        for name in ["alice", "bob"] {
            fx.ok(&[
                "agent",
                "register",
                name,
                "--program",
                "codex",
                "--project",
                ".",
            ]);
        }
        fx.ok(&[
            "send",
            "-f",
            "alice@shop",
            "--to",
            "bob@shop",
            "--subject",
            "hi",
            "--body",
            "b",
        ]);
        fx.ok(&["hold", "acquire", "-a", "alice@shop", "src/a.rs"]);
        fx
    }

    fn ok(&self, args: &[&str]) -> String {
        let (code, out, err) = run(&self.home, &self.shop, args);
        assert_eq!(code, 0, "{args:?}: {err}");
        out
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        serde_json::from_str(&self.ok(&all)).unwrap()
    }

    fn fails(&self, args: &[&str]) -> String {
        let (code, out, err) = run(&self.home, &self.shop, args);
        assert_eq!(code, 1, "{args:?} exited {code}: {out}{err}");
        err
    }
}

#[test]
fn the_summary_names_agents_requests_and_holds() {
    let fx = Fx::new();
    let text = fx.ok(&["activity", "--summary"]);
    assert!(
        text.contains("active agents: alice@shop, bob@shop"),
        "{text}"
    );
    assert!(
        text.contains("requests: 0 open, 0 claimed, 0 stale"),
        "{text}"
    );
    assert!(text.contains("1 live hold(s)"), "{text}");

    let v = fx.json(&["activity", "--summary"]);
    assert_eq!(v["counts"]["mail"], 1, "{v}");
    assert_eq!(v["counts"]["hold"], 1, "{v}");
    assert_eq!(v["counts"]["session"], 2, "{v}");
    assert!(v["since"].is_string() && v["until"].is_string(), "{v}");
}

#[test]
fn kinds_filters_the_feed_and_unknown_kinds_fail() {
    let fx = Fx::new();
    let mail = fx.ok(&["activity", "--kinds", "mail"]);
    assert_eq!(mail.lines().count(), 1, "{mail}");
    assert!(
        mail.contains("alice@shop sent message to bob@shop: hi"),
        "{mail}"
    );
    let hold = fx.ok(&["activity", "--since", "1h", "--kinds", "hold"]);
    assert_eq!(hold.lines().count(), 1, "{hold}");
    assert!(
        hold.contains("alice@shop holds shop:src/a.rs (exclusive)"),
        "{hold}"
    );

    let v = fx.json(&["activity", "--kinds", "mail"]);
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{v}");
    assert_eq!(items[0]["kind"], "message.sent", "{v}");
    assert_eq!(v["next_since"], items[0]["event_id"], "{v}");

    let err = fx.fails(&["activity", "--kinds", "bogus"]);
    assert!(err.contains("unknown activity kind \"bogus\""), "{err}");
}

#[test]
fn since_takes_a_time_or_an_event_cursor_and_refuses_anything_else() {
    let fx = Fx::new();
    let all = fx.json(&["activity", "--since", "2020-01-01T00:00:00Z"]);
    let items = all["items"].as_array().unwrap();
    assert_eq!(items.len(), 6, "{all}");

    let two = fx.ok(&[
        "activity",
        "--since",
        "2020-01-01T00:00:00Z",
        "--limit",
        "2",
    ]);
    assert_eq!(two.lines().count(), 2, "{two}");

    // An event id is a cursor: only what came after it, nothing after the last one.
    let mail = items
        .iter()
        .position(|e| e["kind"] == "message.sent")
        .unwrap();
    let id = items[mail]["event_id"].as_str().unwrap();
    let after = fx.json(&["activity", "--since", id]);
    let after = after["items"].as_array().unwrap();
    assert_eq!(after.len(), items.len() - mail - 1, "{after:?}");
    assert_eq!(after[0]["kind"], "hold.acquired", "{after:?}");
    let last = items.last().unwrap()["event_id"].as_str().unwrap();
    assert_eq!(fx.ok(&["activity", "--since", last]), "");

    let err = fx.fails(&["activity", "--since", "nonsense"]);
    assert!(err.contains("invalid since \"nonsense\""), "{err}");
}

/// `--since` is applied, not only parsed: a later time, or a duration that starts after the
/// last event, leaves nothing in the feed or the summary. A duration counts back from now,
/// not from the epoch (FINDINGS-209).
#[test]
fn since_leaves_out_events_before_it() {
    let fx = Fx::new();
    let future = "2999-01-01T00:00:00Z";
    assert_eq!(fx.ok(&["activity", "--since", future]), "");
    let v = fx.json(&["activity", "--summary", "--since", future]);
    assert_eq!(v["counts"], serde_json::json!({}), "{v}");

    std::thread::sleep(std::time::Duration::from_millis(1_500));
    assert_eq!(fx.ok(&["activity", "--since", "1s"]), "");
    let v = fx.json(&["activity", "--summary", "--since", "1s"]);
    assert_eq!(v["counts"], serde_json::json!({}), "{v}");
    assert_eq!(fx.ok(&["activity", "--since", "1h"]).lines().count(), 6);
}
