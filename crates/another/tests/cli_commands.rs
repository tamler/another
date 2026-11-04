//! CLI commands the r4 coverage pass found no test running: the real binary, its human output,
//! and what actually changed in the store afterwards. Each test has its own ANOTHER_HOME.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use another_core::Store;
use serde_json::Value;

struct Home {
    _dir: tempfile::TempDir,
    home: PathBuf,
    user: PathBuf,
}

fn home() -> Home {
    let dir = tempfile::tempdir().unwrap();
    let (home, user) = (dir.path().join("another"), dir.path().join("user"));
    std::fs::create_dir_all(&user).unwrap();
    let h = Home {
        _dir: dir,
        home,
        user,
    };
    h.text(&["init"]);
    h
}

impl Home {
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_another"))
            .args(args)
            .current_dir(&self.user)
            .env("ANOTHER_HOME", &self.home)
            .env("HOME", &self.user)
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_LANE")
            .env_remove("ANOTHER_LOG")
            .env_remove("ANOTHER_SERVER")
            .env_remove("TMUX")
            .env_remove("CMUX_SOCKET_PATH")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn text(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "another {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        serde_json::from_str(&self.text(&all)).unwrap()
    }

    /// Stderr of a command that must fail.
    fn refused(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(!out.status.success(), "another {args:?} succeeded");
        String::from_utf8_lossy(&out.stderr).to_string()
    }

    fn store(&self) -> Store {
        Store::open(&self.home).unwrap()
    }
}

fn has(text: &str, wanted: &[&str]) {
    for w in wanted {
        assert!(text.contains(w), "missing {w:?} in:\n{text}");
    }
}

/// `claude@shop` in a project at `/work/shop`, and a person.
fn with_agents() -> Home {
    let h = home();
    h.text(&["project", "ensure", "/work/shop"]);
    for args in [
        &[
            "agent",
            "register",
            "claude",
            "--program",
            "claude",
            "--project",
            "/work/shop",
        ][..],
        &["agent", "register", "pat", "--program", "human"][..],
    ] {
        h.text(args);
    }
    h
}

#[test]
fn project_ensure_uses_the_path_or_the_current_directory_and_list_shows_them() {
    let h = home();
    let named = h.text(&["project", "ensure", "/work/shop"]);
    has(&named, &["shop", "/work/shop"]);

    let here = h.json(&["project", "ensure"]);
    let path = here["abs_path"].as_str().unwrap();
    let user = std::fs::canonicalize(&h.user).unwrap();
    let path_ref = std::path::Path::new(path);
    assert!(path_ref == h.user || path_ref == user, "{path}");

    let listed = h.text(&["project", "list"]);
    has(&listed, &["/work/shop", path]);
    assert_eq!(h.json(&["project", "list"]).as_array().unwrap().len(), 2);
}

#[test]
fn config_lists_reads_and_changes_settings() {
    let h = with_agents();
    let all = h.text(&["config"]);
    has(&all, &["decider = ", "(default)", "  # "]);

    let set = h.text(&["config", "decider", "pat"]);
    has(&set, &["decider = pat"]);
    assert!(!set.contains("(default)"), "{set}");
    has(&h.text(&["config", "decider"]), &["decider = pat"]);
    assert_eq!(h.json(&["config", "decider"])[0]["value"], "pat");

    has(&h.refused(&["config", "no_such_setting"]), &["no_such"]);
}

#[test]
fn spend_account_update_and_archive_change_the_account() {
    let h = with_agents();
    h.text(&[
        "spend", "account", "add", "gpu", "--kind", "gpu_pod", "--unit", "USD", "--budget", "100",
    ]);
    let updated = h.text(&[
        "spend",
        "account",
        "update",
        "gpu",
        "--budget",
        "250",
        "--reset-day",
        "5",
        "--cost-per-hour",
        "1.5",
        "--requires-approval",
        "true",
        "--approver",
        "pat",
        "--description",
        "training box",
    ]);
    has(&updated, &["gpu", "250"]);
    let a = &h.json(&["spend", "account", "list"])[0];
    assert_eq!(a["budget"], 250.0);
    assert_eq!(a["reset_day"], 5);
    assert_eq!(a["cost_per_hour"], 1.5);
    assert_eq!(a["requires_approval"], true);
    assert_eq!(a["description"], "training box");

    h.text(&["spend", "account", "update", "gpu", "--clear-budget"]);
    assert!(h.json(&["spend", "account", "list"])[0]["budget"].is_null());

    has(
        &h.text(&["spend", "account", "archive", "gpu"]),
        &["archived spend account gpu"],
    );
    assert_eq!(
        h.json(&["spend", "account", "list"])
            .as_array()
            .unwrap()
            .len(),
        0,
        "archived accounts leave the list"
    );
    has(
        &h.refused(&["spend", "account", "update", "nope", "--budget", "1"]),
        &["nope"],
    );
}

#[test]
fn decision_cancel_withdraws_it_for_the_asker_only() {
    let h = with_agents();
    h.text(&["agent", "register", "other", "--program", "codex"]);
    let asked = h.text(&[
        "decision",
        "ask",
        "--agent",
        "claude@shop",
        "--to",
        "pat",
        "--question",
        "Ship it?",
        "--option",
        "yes=Ship it",
        "--option",
        "no=Hold off",
    ]);
    has(&asked, &["asked pat", "Ship it?", "Ship it", "Hold off"]);
    let id = h.json(&["decision", "list"])[0]["decision_id"]
        .as_str()
        .unwrap()
        .to_string();

    has(&h.text(&["decision", "show", &id]), &[&id[..], "Ship it?"]);
    h.refused(&["decision", "cancel", &id, "--agent", "other"]);
    has(
        &h.text(&[
            "decision",
            "cancel",
            &id,
            "--agent",
            "claude@shop",
            "--reason",
            "moot",
        ]),
        &["withdrew decision", &id[..]],
    );
    assert_eq!(h.json(&["decision", "show", &id])["status"], "cancelled");
    // It can't be withdrawn twice.
    h.refused(&["decision", "cancel", &id, "--agent", "claude@shop"]);
}

#[test]
fn hold_renew_extends_the_agents_holds() {
    let h = with_agents();
    let got = h.json(&[
        "hold",
        "acquire",
        "--agent",
        "claude@shop",
        "--project",
        "shop",
        "--ttl",
        "120",
        "src/api.rs",
    ]);
    let hold = got["granted"][0]["hold_id"].as_str().unwrap().to_string();
    let before = got["granted"][0]["expires_at"]
        .as_str()
        .unwrap()
        .to_string();
    std::thread::sleep(std::time::Duration::from_millis(1100));

    let renewed = h.text(&["hold", "renew", "--agent", "claude@shop", &hold]);
    has(&renewed, &["src/api.rs"]);
    let after = h.json(&["hold", "renew", "--agent", "claude@shop"]);
    assert!(
        after[0]["expires_at"].as_str().unwrap() > before.as_str(),
        "{after}"
    );
}

#[test]
fn webhook_retry_requeues_a_dead_delivery() {
    let h = with_agents();
    h.text(&[
        "webhook",
        "add",
        "--url",
        "http://127.0.0.1:9/hook",
        "--events",
        "lane_status",
        "--all-projects",
    ]);
    h.text(&[
        "lane",
        "create",
        "api",
        "--agent",
        "claude@shop",
        "--project",
        "shop",
        "--lead",
        "claude@shop",
        "--charter",
        "c",
    ]);
    let s = h.store();
    s.queue_webhook_deliveries().unwrap();
    let id = s.claim_due_deliveries(1).unwrap()[0].delivery_id.clone();
    // A delivery can only be retried once it is dead: use up its attempts.
    let mut attempts = 0;
    while s
        .webhook_deliveries(None, Some(another_core::DeliveryStatus::Dead), 5)
        .unwrap()
        .is_empty()
    {
        s.record_delivery(&id, Err("connection refused".into()))
            .unwrap();
        attempts += 1;
        assert!(attempts < 50, "the delivery never went dead");
    }

    has(
        &h.text(&["webhook", "retry", &id]),
        &["re-queued delivery", &id[..], "another serve"],
    );
    let list = h.json(&["webhook", "deliveries"]);
    assert_eq!(list[0]["status"], "pending", "{list}");
    h.refused(&["webhook", "retry", "no-such-delivery"]);
}

#[test]
fn summary_schedule_and_wake_explain_what_is_missing() {
    let h = with_agents();
    has(
        &h.refused(&["summary", "schedule", "--to", "mail:pat"]),
        &["--daily"],
    );
    has(&h.refused(&["wake", "--dry-run"]), &["--agent", "--all"]);
}

/// Never passes `--confirm`, so nothing is signalled. The listener is this test process's own.
#[test]
fn reap_plans_a_listener_and_refuses_one_a_resource_registered() {
    let h = with_agents();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let target = format!("pid:{}", std::process::id());

    let plan = h.json(&["reap", &target, "--grace", "0"]);
    assert_eq!(plan["executed"], false);
    let found = plan["blockers"]
        .as_array()
        .unwrap()
        .iter()
        .all(|b| !b.as_str().unwrap().contains("not a listening process"));
    if !found {
        // No lsof or ss on this machine: nothing to observe.
        return;
    }
    assert_eq!(plan["eligible"], true, "{plan}");
    has(
        &h.text(&["reap", &target, "--grace", "0"]),
        &["eligible; nothing was done", "--confirm"],
    );

    h.text(&[
        "resource",
        "register",
        "--agent",
        "claude@shop",
        "model_server",
        "mine",
        "--project",
        "shop",
        "--port",
        &port,
        "--ttl",
        "600",
    ]);
    let plan = h.json(&["reap", &target, "--grace", "0"]);
    assert_eq!(plan["eligible"], false, "{plan}");
    has(
        &h.text(&["reap", &target, "--grace", "0"]),
        &["blocked: ", "belongs to a registered resource"],
    );
    drop(listener);
}
