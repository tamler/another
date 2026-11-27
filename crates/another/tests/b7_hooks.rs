use std::sync::{
    atomic::{AtomicI64, Ordering},
    Arc,
};

use another::hooks::{self, Client, HookEnv, HookEvent, HookInput};
use another_core::{Clock, Store, SubjectKind};
use serde_json::{json, Value};

struct Fixture {
    dir: tempfile::TempDir,
    store: Store,
    env: HookEnv,
    now: Arc<AtomicI64>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (clock, now) = Clock::manual(1_800_000_000_000);
        let home = dir.path().join("state");
        let store = Store::open_with_clock(&home, clock).unwrap();
        let env = HookEnv {
            home: dir.path().join("user"),
            another_home: home,
            exe: std::env::current_exe().unwrap(),
        };
        Self {
            dir,
            store,
            env,
            now,
        }
    }
    fn event(&self, client: Client, event: HookEvent, mut payload: Value) -> String {
        payload["cwd"] = json!(self.dir.path());
        hooks::run(
            &self.store,
            &self.env,
            event,
            client,
            HookInput::parse(&payload.to_string()),
        )
        .unwrap()
    }
    fn pre(&self, session: &str, child: Option<&str>, call: &str, file: &str) -> String {
        self.event(Client::Claude, HookEvent::PreEdit, json!({"session_id":session, "agent_id":child, "tool_use_id":call, "hook_event_name":"PreToolUse", "tool_name":"Write", "tool_input":{"file_path":file}}))
    }
}

#[test]
fn grok_child_end_closes_only_its_dependent_branch() {
    let f = Fixture::new();
    f.event(
        Client::Grok,
        HookEvent::SessionStart,
        json!({"sessionId":"root"}),
    );
    for child in ["child", "sibling"] {
        f.event(
            Client::Grok,
            HookEvent::ChildStart,
            json!({"sessionId":"root", "subagentId":child}),
        );
        let out = f.event(Client::Grok, HookEvent::PreEdit, json!({"sessionId":child, "tool_use_id":child, "tool_name":"Write", "tool_input":{"file_path":child}}));
        assert!(out.is_empty(), "{out}");
    }
    f.event(
        Client::Grok,
        HookEvent::SessionEnd,
        json!({"sessionId":"child"}),
    );
    let leases = f.store.right_leases().unwrap();
    let file = |name: &str| {
        leases
            .iter()
            .find(|l| {
                l.subject_kind == SubjectKind::File
                    && std::path::Path::new(&l.subject_key).ends_with(name)
            })
            .unwrap()
    };
    assert_eq!(file("child").release_reason.as_deref(), Some("session_end"));
    assert!(file("sibling").released_at.is_none());
    f.event(
        Client::Grok,
        HookEvent::SessionEnd,
        json!({"sessionId":"root"}),
    );
    assert!(f
        .store
        .right_leases()
        .unwrap()
        .iter()
        .all(|l| l.released_at.is_some()));
}

#[test]
fn missing_identity_and_unknown_shell_refuse_without_name_authority() {
    let f = Fixture::new();
    let denied = f.event(
        Client::Claude,
        HookEvent::PreEdit,
        json!({"tool_name":"Write", "tool_input":{"file_path":"A"}}),
    );
    assert_eq!(
        serde_json::from_str::<Value>(&denied).unwrap()["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );
    assert!(f.store.right_leases().unwrap().is_empty());
    let denied = f.event(Client::Claude, HookEvent::PreEdit, json!({"session_id":"root", "tool_name":"Bash", "tool_use_id":"shell", "tool_input":{"command":"echo write > A"}}));
    assert!(denied.contains("NO_CALL_CONTEXT"));
    assert!(!f
        .store
        .right_leases()
        .unwrap()
        .iter()
        .any(|l| l.subject_kind == SubjectKind::File));
}

/// PR #296 review P2: a tool Another does not know is refused even when it carries a
/// familiar path key, because its other fields may name more targets.
#[test]
fn an_unknown_write_tool_with_a_familiar_path_key_is_refused() {
    let f = Fixture::new();
    let out = f.event(
        Client::Claude,
        HookEvent::PreEdit,
        json!({"session_id":"root", "tool_use_id":"unknown-call", "tool_name":"UnknownWriteTool", "tool_input":{"file_path":"A", "other_target":"B"}}),
    );
    assert_eq!(
        serde_json::from_str::<Value>(&out).unwrap()["hookSpecificOutput"]["permissionDecision"],
        "deny",
        "{out}"
    );
    assert!(out.contains("NO_CALL_CONTEXT"), "{out}");
    assert!(!f
        .store
        .right_leases()
        .unwrap()
        .iter()
        .any(|l| l.subject_kind == SubjectKind::File));
    // A known write tool with the same path is still admitted.
    assert!(f.pre("root", None, "known-call", "A").is_empty());
}

#[test]
fn exact_completion_renews_b_but_denial_and_presence_do_not_renew_a() {
    let f = Fixture::new();
    assert!(f.pre("root", None, "A", "A").is_empty());
    let a = f
        .store
        .right_leases()
        .unwrap()
        .into_iter()
        .find(|l| l.subject_kind == SubjectKind::File)
        .unwrap();
    f.now.fetch_add(70_000, Ordering::SeqCst);
    assert!(f.pre("root", None, "repeat-A", "A").is_empty());
    f.event(Client::Claude, HookEvent::PostTool, json!({"session_id":"root", "tool_use_id":"A", "hook_event_name":"PostToolUseFailure", "failure_type":"permission_denied"}));
    assert!(f.pre("root", None, "B", "B").is_empty());
    f.now.fetch_add(10_000, Ordering::SeqCst);
    assert!(f
        .event(
            Client::Claude,
            HookEvent::PostTool,
            json!({"session_id":"root", "tool_use_id":"B", "hook_event_name":"PostToolUse"})
        )
        .is_empty());
    let leases = f.store.right_leases().unwrap();
    assert_eq!(
        leases.iter().find(|l| l.lease_id == a.lease_id).unwrap(),
        &a
    );
    assert!(leases
        .iter()
        .any(|l| l.subject_key.to_ascii_lowercase().ends_with('b')
            && l.last_update_at == f.store.now()));
}

#[test]
fn root_end_releases_children_and_late_failure_never_revives() {
    let f = Fixture::new();
    assert!(f.pre("root", None, "root-call", "A").is_empty());
    assert!(f.pre("root", Some("child"), "child-call", "B").is_empty());
    assert_eq!(f.store.contract_actors().unwrap().len(), 2);
    f.event(
        Client::Claude,
        HookEvent::SessionEnd,
        json!({"session_id":"root", "hook_event_name":"SessionEnd"}),
    );
    assert!(f
        .store
        .right_leases()
        .unwrap()
        .iter()
        .all(|l| l.released_at.is_some()));
    let result = hooks::run(&f.store, &f.env, HookEvent::PostTool, Client::Claude, HookInput::parse(&json!({"session_id":"root", "agent_id":"child", "tool_use_id":"child-call", "hook_event_name":"PostToolUseFailure"}).to_string()));
    // A closed epoch may reject presence after recording late outcome history.
    assert!(result.is_err() || result.unwrap().is_empty());
    assert!(f
        .store
        .right_leases()
        .unwrap()
        .iter()
        .all(|l| l.released_at.is_some()));
    assert!(f.pre("root", None, "late", "C").contains("deny"));
}

#[test]
fn idle_reminder_is_once_and_cannot_extend_file_deadlines() {
    let f = Fixture::new();
    f.pre("root", None, "A", "A");
    let before = f
        .store
        .right_leases()
        .unwrap()
        .into_iter()
        .find(|l| l.subject_kind == SubjectKind::File)
        .unwrap();
    f.now.fetch_add(120_000, Ordering::SeqCst);
    let returning = f.event(
        Client::Claude,
        HookEvent::Prompt,
        json!({"session_id":"root"}),
    );
    assert!(returning.contains("You were inactive"));
    assert!(f
        .event(
            Client::Claude,
            HookEvent::Prompt,
            json!({"session_id":"root"})
        )
        .is_empty());
    let after = f
        .store
        .right_leases()
        .unwrap()
        .into_iter()
        .find(|l| l.lease_id == before.lease_id)
        .unwrap();
    assert_eq!(before.expires_at, after.expires_at);
}

#[test]
fn cursor_stays_quiet_and_has_no_shell_mutation_route() {
    let f = Fixture::new();
    assert!(f
        .event(
            Client::Cursor,
            HookEvent::SessionStart,
            json!({"session_id":"root", "conversation_id":"root", "hook_event_name":"sessionStart"})
        )
        .is_empty());
    let denied = f.event(Client::Cursor, HookEvent::PreEdit, json!({"session_id":"root", "conversation_id":"root", "tool_name":"Shell", "tool_use_id":"shell"}));
    assert!(denied.contains("NO_ACTOR"));
    assert_eq!(
        serde_json::from_str::<Value>(&denied).unwrap()["permission"],
        "deny"
    );
    assert!(!f
        .store
        .right_leases()
        .unwrap()
        .iter()
        .any(|l| l.subject_kind == SubjectKind::File));
}

#[test]
fn names_and_environment_cannot_authorize_cli_mutations() {
    let f = Fixture::new();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["--json", "hold", "acquire", "--agent", "claude", "A"])
        .env("ANOTHER_HOME", &f.env.another_home)
        .env("ANOTHER_AGENT", "claude")
        .current_dir(f.dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("NO_ACTOR"));
    assert!(f.store.right_leases().unwrap().is_empty());
}
