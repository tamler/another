use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::Instant;

use rusqlite::Connection;
use serde_json::{json, Value};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let home = temp.path().join("home");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(root.join(".git/config"), "[core]\n bare = false\n").unwrap();
        // Hooks never create a home: it exists before any session starts.
        assert!(Command::new(env!("CARGO_BIN_EXE_another"))
            .arg("init")
            .env("ANOTHER_HOME", home.join(".another"))
            .output()
            .unwrap()
            .status
            .success());
        Self {
            _temp: temp,
            root,
            home,
        }
    }

    fn hook(&self, event: &str, input: Value) -> Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_another"))
            .args(["hook", "run", event, "--client", "claude"])
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("ANOTHER_HOME", self.home.join(".another"))
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
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn start(&self, session: &str) -> Output {
        self.hook(
            "session-start",
            json!({"session_id":session, "cwd":self.root}),
        )
    }

    fn db(&self) -> Connection {
        Connection::open(self.home.join(".another/another.db")).unwrap()
    }
}

#[test]
fn identity_lock_contention_denies_pre_edit() {
    let env = Fixture::new();
    let start = env.start("existing-session");
    assert!(
        start.status.success() && String::from_utf8_lossy(&start.stdout).starts_with("Another: "),
        "setup: {start:?}"
    );
    let db = env.db();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    let began = Instant::now();
    let output = env.hook("pre-edit", json!({"session_id":"existing-session", "cwd":env.root,
        "hook_event_name":"PreToolUse", "tool_name":"Edit", "tool_input":{"file_path":env.root.join("src/a.rs")}}));
    let elapsed = began.elapsed();
    db.execute_batch("ROLLBACK").unwrap();
    let holds: i64 = db
        .query_row("SELECT count(*) FROM holds", [], |r| r.get(0))
        .unwrap();
    let reply = serde_json::from_slice::<Value>(&output.stdout);
    assert!(output.status.success() && reply.as_ref().ok().is_some_and(|v| v["hookSpecificOutput"]["permissionDecision"] == "deny") && holds == 0,
        "locked identity pre-edit must visibly deny: elapsed={elapsed:?}, holds={holds}, status={:?}, stdout={:?}, stderr={:?}",
        output.status.code(), String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}

#[test]
fn missing_identity_uniqueness_index_refuses_start_without_mutation() {
    let env = Fixture::new();
    let start = env.start("first-session");
    assert!(
        start.status.success() && String::from_utf8_lossy(&start.stdout).starts_with("Another: "),
        "setup: {start:?}"
    );
    let db = env.db();
    db.execute_batch("DROP INDEX agents_identity").unwrap();
    let counts = || -> (i64, i64, i64) {
        (
            db.query_row("SELECT count(*) FROM agents", [], |r| r.get(0))
                .unwrap(),
            db.query_row("SELECT count(*) FROM events", [], |r| r.get(0))
                .unwrap(),
            db.query_row(
                "SELECT count(*) FROM events WHERE kind='session.binding.v1'",
                [],
                |r| r.get(0),
            )
            .unwrap(),
        )
    };
    let before = counts();
    let output = env.start("second-session");
    let after = counts();
    assert!(before == after && !String::from_utf8_lossy(&output.stdout).starts_with("Another: "),
        "missing uniqueness index must refuse before writes: before={before:?}, after={after:?}, status={:?}, stdout={:?}, stderr={:?}",
        output.status.code(), String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}
