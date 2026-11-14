use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("shop");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join(".git/config"), "[core]\n\tbare = false\n").unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        Self {
            _dir: dir,
            root,
            home,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_another"));
        cmd.args(args)
            .env("HOME", &self.home)
            .env("ANOTHER_HOME", self.home.join(".another"))
            .env_remove("ANOTHER_AGENT")
            .env_remove("GROK_HOOK_EVENT")
            .env_remove("HERDR_PANE_ID")
            .env_remove("TMUX_PANE")
            .env_remove("CMUX_SURFACE_ID");
        cmd
    }

    fn hook(&self, event: &str, input: &Value) -> String {
        let mut child = self
            .command(&["hook", "run", event, "--client", "claude"])
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
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn start(&self, session: &str) {
        self.hook(
            "session-start",
            &json!({"session_id": session, "cwd": self.root}),
        );
    }

    fn edit(&self, session: &str, path: &Path) -> String {
        self.hook(
            "pre-edit",
            &json!({"session_id": session, "cwd": self.root,
            "hook_event_name": "PreToolUse", "tool_name": "Edit",
            "tool_input": {"file_path": path}}),
        )
    }

    fn holds(&self) -> Value {
        let output = self.command(&["--json", "holds"]).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

#[test]
fn concurrent_sessions_of_same_client_conflict_on_the_same_file() {
    let env = Fixture::new();
    env.start("session-a");
    env.start("session-b");
    let path = env.root.join("src/shared.rs");
    assert_eq!(env.edit("session-a", &path), "");
    let blocked = env.edit("session-b", &path);
    assert!(
        blocked.contains("deny"),
        "second session was not denied: {blocked:?}"
    );
}

#[test]
fn delayed_hook_end_does_not_release_resumed_session_hold() {
    let env = Fixture::new();
    env.start("resumed-session");
    env.start("resumed-session");
    assert_eq!(
        env.edit("resumed-session", &env.root.join("src/resumed.rs")),
        ""
    );
    assert_eq!(env.holds().as_array().unwrap().len(), 1);
    env.hook(
        "session-end",
        &json!({"session_id": "resumed-session", "cwd": env.root}),
    );
    assert_eq!(
        env.holds().as_array().unwrap().len(),
        1,
        "delayed End released the resumed hold"
    );
}
