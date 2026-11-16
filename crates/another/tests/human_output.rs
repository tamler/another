//! The human (non-JSON) renderers in output.rs that no other test reached: agents, sent
//! mail, threads, heartbeat, events, tokens, audit, peers and wake targets. Each command runs
//! the real binary and must succeed with readable text. The token checks are the reason
//! this matters: `token list` and `audit` never show a token's secret.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

struct Fx {
    _dir: tempfile::TempDir,
    home: PathBuf,
    shop: PathBuf,
}

impl Fx {
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
        for (name, program) in [("alice", "claude"), ("bob", "codex")] {
            fx.ok(&[
                "agent",
                "register",
                name,
                "--program",
                program,
                "--project",
                ".",
            ]);
        }
        fx
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_another"))
            .args(args)
            .current_dir(&self.shop)
            .env("ANOTHER_HOME", &self.home)
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_LOG")
            .env_remove("HERDR_PANE_ID")
            .env_remove("TMUX_PANE")
            .env_remove("CMUX_SURFACE_ID")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// Human output of a command that must succeed.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        serde_json::from_str(&self.ok(&all)).unwrap()
    }

    fn shop(&self) -> &str {
        self.shop.to_str().unwrap()
    }
}

fn path_str(p: &Path) -> &str {
    p.to_str().unwrap()
}

#[test]
fn agents_mail_threads_heartbeat_and_events_read_as_text() {
    let fx = Fx::new();
    let agents = fx.ok(&["agent", "list"]);
    assert!(agents.contains("alice@shop"), "{agents}");
    assert!(agents.contains("bob@shop"), "{agents}");

    let sent = fx.ok(&[
        "send",
        "--from",
        "alice@shop",
        "--to",
        "bob@shop",
        "--subject",
        "Plan for today",
        "--body",
        "first",
    ]);
    assert!(!sent.trim().is_empty(), "send printed nothing");
    let thread_id = fx.json(&["threads", "--agent", "bob@shop"])[0]["thread_id"]
        .as_str()
        .expect("a thread id")
        .to_string();

    let threads = fx.ok(&["threads", "--agent", "bob@shop"]);
    assert!(threads.contains("Plan for today"), "{threads}");
    let thread = fx.ok(&["thread", &thread_id]);
    assert!(thread.contains("Plan for today"), "{thread}");
    assert!(
        thread.contains("first"),
        "the thread shows the body: {thread}"
    );

    let beat = fx.ok(&["heartbeat", "--agent", "alice@shop"]);
    assert!(!beat.trim().is_empty(), "heartbeat printed nothing");

    let events = fx.ok(&["events", "--limit", "50"]);
    assert!(events.contains("message.sent"), "{events}");
    assert!(events.lines().count() >= 3, "{events}");
}

#[test]
fn token_list_and_audit_never_show_the_secret() {
    let fx = Fx::new();
    let created = fx.json(&[
        "token",
        "create",
        "--agent",
        "alice@shop",
        "--project",
        fx.shop(),
        "--label",
        "ci",
    ]);
    let secret = created["secret"]
        .as_str()
        .expect("token create returns its secret once")
        .to_string();
    let token_id = created["token_id"]
        .as_str()
        .expect("a token id")
        .to_string();
    // The secret's hex part, in case a renderer printed it without its prefix.
    let tail = secret.rsplit('_').next().unwrap().to_string();
    assert!(tail.len() >= 32, "{secret}");

    let list = fx.ok(&["token", "list"]);
    assert!(list.contains(&token_id), "{list}");
    assert!(list.contains("active"), "{list}");
    assert!(list.contains("(ci)"), "{list}");
    assert!(list.contains(path_str(&fx.shop)), "{list}");
    assert!(
        !list.contains(&tail),
        "token list printed the secret: {list}"
    );

    fx.ok(&["token", "revoke", &token_id]);
    let list = fx.ok(&["token", "list"]);
    assert!(list.contains("revoked"), "{list}");

    let audit = fx.ok(&["audit"]);
    assert!(!audit.contains(&tail), "audit printed the secret: {audit}");
}

#[test]
fn peers_and_wake_targets_say_when_there_are_none() {
    let fx = Fx::new();
    assert_eq!(fx.ok(&["peers"]).trim(), "not paired with any node");
    let wake = fx.ok(&["wake", "list"]);
    assert!(wake.starts_with("no wake targets"), "{wake}");

    fx.ok(&[
        "wake", "set", "--agent", "bob@shop", "--via", "tmux", "--target", "%3",
    ]);
    let wake = fx.ok(&["wake", "list"]);
    assert!(wake.contains("bob@shop"), "{wake}");
    assert!(wake.contains("%3"), "{wake}");
}
