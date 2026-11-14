//! Public CLI regressions for per-edit admission. Payloads are synthetic; actual client
//! propagation and enforcement require separate integration evidence.
#![cfg(unix)]

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use another_core::{controlled_sha256, ControlledEdit, Store};
use serde_json::{json, Value};

struct Fixture {
    _temp: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
    actor: String,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(project.join(".git/config"), "[core]\n bare = false\n").unwrap();
        std::fs::write(project.join("src/a.rs"), "before").unwrap();
        let project = std::fs::canonicalize(project).unwrap();
        let mut f = Self {
            _temp: temp,
            home,
            project,
            actor: String::new(),
        };
        assert!(f
            .command(&["init"])
            .stdin(Stdio::null())
            .output()
            .unwrap()
            .status
            .success());
        let start = f.hook(
            "session-start",
            &json!({"session_id":"session-a", "cwd":f.project}),
        );
        assert!(start.starts_with("Another: "), "{start}");
        f.actor = start
            .strip_prefix("Another: ")
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned();
        f
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_another"));
        c.args(args)
            .current_dir(&self.project)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("ANOTHER_HOME", self.home.join(".another"))
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_AGENT_MODE")
            .env_remove("GROK_HOOK_EVENT")
            .env_remove("HERDR_PANE_ID")
            .env_remove("TMUX_PANE")
            .env_remove("CMUX_SURFACE_ID");
        c
    }

    fn hook(&self, event: &str, payload: &Value) -> String {
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
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    fn pre_edit(&self, id: Option<&str>) -> String {
        let mut payload = json!({"session_id":"session-a", "cwd":self.project,
            "hook_event_name":"PreToolUse", "tool_name":"Edit",
            "tool_input":{"file_path":self.project.join("src/a.rs")}});
        if let Some(id) = id {
            payload["tool_use_id"] = json!(id);
        }
        self.hook("pre-edit", &payload)
    }

    fn edit(&self) -> ControlledEdit {
        ControlledEdit {
            project: std::fs::canonicalize(&self.project)
                .unwrap()
                .to_str()
                .unwrap()
                .into(),
            path: "src/a.rs".into(),
            expected_sha256: controlled_sha256(b"before"),
            replacement: "after".into(),
        }
    }

    fn controlled(&self, id: Option<&str>) -> String {
        let mut payload = json!({"session_id":"session-a","cwd":self.project,
            "tool_name":"mcp__another__controlled_edit","tool_input":self.edit()});
        if let Some(id) = id {
            payload["tool_use_id"] = json!(id);
        }
        self.hook("pre-controlled-edit", &payload)
    }
}

fn assert_denied(output: &str) {
    let value: Value = serde_json::from_str(output).expect("pre-edit must emit denial JSON");
    assert_eq!(value["hookSpecificOutput"]["permissionDecision"], "deny");
}

#[test]
fn protected_edit_without_tool_id_is_denied() {
    let f = Fixture::new();
    assert_denied(&f.pre_edit(None));
    assert_denied(&f.controlled(None));
}

#[test]
fn duplicate_tool_id_cannot_authorize_a_second_edit() {
    let f = Fixture::new();
    assert_eq!(
        f.controlled(Some("call-1")),
        "",
        "first edit should be admitted"
    );
    assert_denied(&f.controlled(Some("call-1")));
}

#[test]
fn distinct_edits_of_one_file_get_distinct_pending_holds() {
    let f = Fixture::new();
    assert_eq!(f.controlled(Some("call-a")), "");
    assert_eq!(f.controlled(Some("call-b")), "");
    let db = rusqlite::Connection::open(f.home.join(".another/another.db")).unwrap();
    let mut query = db
        .prepare(
            "SELECT a.state, h.hold_id, h.pattern FROM admissions a
         JOIN admission_holds ah ON ah.admission_id = a.admission_id
         JOIN holds h ON h.hold_id = ah.hold_id ORDER BY h.hold_id",
        )
        .expect("v9 admission schema must exist");
    let rows: Vec<(String, String, String)> = query
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows
        .iter()
        .all(|row| row.0 == "pending" && row.2 == "src/a.rs"));
    assert_ne!(rows[0].1, rows[1].1);
}

#[test]
fn admission_does_not_renew_or_replace_an_existing_manual_hold() {
    let f = Fixture::new();
    let acquired = f
        .command(&[
            "hold", "acquire", "--agent", &f.actor, "--ttl", "1800", "src/a.rs",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(acquired.status.success(), "{acquired:?}");
    let db = rusqlite::Connection::open(f.home.join(".another/another.db")).unwrap();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    db.execute(
        "UPDATE holds SET expires_at=?1 WHERE pattern='src/a.rs'",
        [now_ms + 901_000],
    )
    .unwrap();
    let original: (String, i64, Option<i64>) = db
        .query_row(
            "SELECT hold_id, expires_at, released_at FROM holds WHERE pattern='src/a.rs'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(f.controlled(Some("separate-edit")), "");
    let retained: (String, i64, Option<i64>) = db
        .query_row(
            "SELECT hold_id, expires_at, released_at FROM holds WHERE hold_id=?1",
            [&original.0],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(retained, original, "manual hold must be unchanged");
    let pending: i64 = db.query_row(
        "SELECT count(*) FROM admission_holds ah JOIN holds h ON h.hold_id=ah.hold_id WHERE h.pattern='src/a.rs' AND h.hold_id != ?1",
        [&original.0], |r| r.get(0)).expect("v9 admission schema must exist");
    assert_eq!(pending, 1, "edit must have its own pending hold");
}

#[test]
fn only_matching_successful_completion_confirms_pending_hold_once() {
    let f = Fixture::new();
    assert_eq!(f.controlled(Some("call-a")), "");
    let db = rusqlite::Connection::open(f.home.join(".another/another.db")).unwrap();
    let read = || -> (String, String, i64) {
        db.query_row(
            "SELECT a.state, h.hold_id, h.expires_at FROM admissions a
             JOIN admission_holds ah ON ah.admission_id=a.admission_id
             JOIN holds h ON h.hold_id=ah.hold_id",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("one pending admission must exist")
    };
    let original = read();
    assert_eq!(original.0, "pending");
    let receipt = |session: &str, id: &str| {
        json!({
            "session_id":session, "cwd":f.project, "tool_use_id":id,
            "hook_event_name":"PostToolUse", "tool_name":"Edit",
            "tool_input":{"file_path":f.project.join("src/a.rs")},
            "tool_response":{"success":true}
        })
    };
    f.hook("post-tool", &receipt("session-a", "wrong-call"));
    assert_eq!(
        read(),
        original,
        "wrong call must not mutate pending coverage"
    );
    f.hook("post-tool", &receipt("wrong-session", "call-a"));
    assert_eq!(
        read(),
        original,
        "wrong session must not mutate pending coverage"
    );
    let mut failed = receipt("session-a", "call-a");
    failed["hook_event_name"] = json!("PostToolUseFailure");
    failed["tool_response"] = json!({"error":"synthetic failure"});
    f.hook("post-tool", &failed);
    assert_eq!(
        read(),
        original,
        "a failed tool call must not confirm coverage"
    );
    f.hook("post-tool", &receipt("session-a", "call-a"));
    assert_eq!(
        read(),
        original,
        "a hook assertion must never confirm a controlled edit"
    );
    let store = Store::open(&f.home.join(".another")).unwrap();
    assert!(another::controlled_edit::execute(&store, "claude", "wrong-call", &f.edit()).is_err());
    assert!(another::controlled_edit::execute(&store, "claude", "call-a", &f.edit()).is_ok());
    assert_eq!(std::fs::read(f.project.join("src/a.rs")).unwrap(), b"after");
    let confirmed = read();
    assert_eq!(confirmed.0, "confirmed");
    assert_eq!(
        confirmed.1, original.1,
        "confirmation must target the exact hold"
    );
    f.hook("post-tool", &receipt("session-a", "call-a"));
    assert_eq!(
        read(),
        confirmed,
        "duplicate receipt must not renew or mutate coverage"
    );
}
