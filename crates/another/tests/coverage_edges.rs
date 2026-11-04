//! Edge cases for the least-covered public functions found by the v0.1.0 coverage pass
//! (`cargo llvm-cov`): mirrored decisions by id, the remote heartbeat route, the saved
//! remote profile, fail-open hooks and the session digest. Tests only.

use std::io::Write;
use std::net::SocketAddr;
use std::process::{Command, Output, Stdio};

use another::fed;
use another::serve::{http_router, HttpOptions};
use another_core::{
    AcquireHolds, AnswerDecision, DecisionOption, DecisionStatus, Importance, JoinClient,
    NewAgentInvite, NewDecision, NewLane, NewRequest, Perm, Program, RegisterAgent, RequestAction,
    RequestUpdate, SendMessage, Store,
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_another")
}

fn agent(s: &Store, name: &str, program: Program, project: Option<&str>) {
    s.register_agent_trusted(RegisterAgent {
        name: name.into(),
        program,
        project: project.map(Into::into),
        model: None,
        description: None,
    })
    .unwrap();
}

fn options() -> Vec<DecisionOption> {
    ["yes", "no"]
        .map(|v| DecisionOption {
            value: v.into(),
            label: v.into(),
            description: None,
        })
        .to_vec()
}

struct Node {
    _dir: tempfile::TempDir,
    store: Store,
    url: String,
    ct: CancellationToken,
    _rt: tokio::runtime::Runtime,
}

impl Drop for Node {
    fn drop(&mut self) {
        self.ct.cancel();
    }
}

fn node(name: &str, auth: bool, federation: bool) -> Node {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.set_node_name(name).unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let ct = CancellationToken::new();
    let app = http_router(
        store.clone(),
        ct.clone(),
        &HttpOptions {
            auth,
            federation,
            ..Default::default()
        },
    );
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = ct.clone();
    rt.spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { stop.cancelled_owned().await })
        .await
        .unwrap();
    });
    Node {
        _dir: dir,
        store,
        url: format!("http://{addr}"),
        ct,
        _rt: rt,
    }
}

// ---------------------------------------------------------------------------------------
// Store::decision with mirrors (a decision asked on another node, looked up by id here)

#[test]
fn mirrored_decisions_are_found_by_id_only_within_scope() {
    let hub = node("hub", true, true);
    let spoke = node("laptop", false, false);
    hub.store.ensure_project("t", "/work/shop").unwrap();
    hub.store.ensure_project("t", "/work/vault").unwrap();
    agent(&hub.store, "claude", Program::Claude, Some("/work/shop"));
    spoke.store.ensure_project("t", "/src/shop").unwrap();
    agent(&spoke.store, "codex", Program::Codex, Some("/src/shop"));
    spoke.store.ensure_project("t", "/src/garden").unwrap();
    agent(
        &spoke.store,
        "gardener",
        Program::Codex,
        Some("/src/garden"),
    );
    let code = hub
        .store
        .create_invite(&hub.url, &[Perm::ReadFeed, Perm::SendMail], 3600)
        .unwrap();
    fed::join(&spoke.store, &code).unwrap();
    let ask = |agent: &str, q: &str| {
        spoke
            .store
            .ask_decision(
                NewDecision {
                    agent: agent.into(),
                    question: q.into(),
                    options: options(),
                    ..Default::default()
                },
                None,
            )
            .unwrap()
    };
    let in_shop = ask("codex@shop", "Use the hub schema?");
    // A project the hub doesn't have: the mirror has no local project.
    let no_project = ask("gardener@garden", "Plant tomatoes?");
    let r = fed::sync(&spoke.store).unwrap();
    assert!(r.push_rejected.is_empty(), "{:?}", r.push_rejected);

    // Locally on the hub, both mirrors are found by id.
    let m = hub.store.decision(&in_shop.decision_id, None).unwrap();
    assert_eq!(m.node.as_deref(), Some("laptop"));
    assert_eq!(m.asker, "codex@shop@laptop");
    assert!(hub.store.decision(&no_project.decision_id, None).is_ok());

    // A token for shop sees the shop mirror; a token for vault gets not_found, the same as
    // for an unknown id. A mirror with no local project is never visible to a token.
    let scope = |glob: &str| {
        let t = hub
            .store
            .create_token("claude@shop", &[glob.to_string()], None, None)
            .unwrap();
        hub.store.verify_token(&t.secret).unwrap()
    };
    // The mirror keeps its own node's project (path /src/shop on the laptop), so a path glob
    // for the hub's /work/shop doesn't cover it; the slug glob `shop` covers both.
    let shop = scope("shop");
    let shop_path_only = scope("/work/shop");
    let vault = scope("/work/vault");
    assert!(hub
        .store
        .decision(&in_shop.decision_id, Some(&shop))
        .is_ok());
    assert!(hub
        .store
        .decision(&in_shop.decision_id, Some(&shop_path_only))
        .is_err());
    let hidden = hub
        .store
        .decision(&in_shop.decision_id, Some(&vault))
        .unwrap_err();
    let unknown = hub
        .store
        .decision("01ZZZZZZZZZZZZZZZZZZZZZZZZ", Some(&vault))
        .unwrap_err();
    assert_eq!(
        std::mem::discriminant(&hidden),
        std::mem::discriminant(&unknown)
    );
    assert!(hub
        .store
        .decision(&no_project.decision_id, Some(&shop))
        .is_err());
    // Locally there's no scope to fail: a mirror with no local project is still found.
    assert!(hub.store.decision(&no_project.decision_id, None).is_ok());
}

// ---------------------------------------------------------------------------------------
// join::heartbeat (POST /agent/heartbeat) and POST /agent/leave

#[test]
fn remote_heartbeats_need_a_token_and_a_known_phase() {
    let hub = node("hub", true, false);
    hub.store.ensure_project("t", "/work/shop").unwrap();
    let code = hub
        .store
        .create_agent_invite(NewAgentInvite {
            agent: "cloud".into(),
            program: Program::Codex,
            project_globs: vec!["/work/shop".into()],
            ..persistent_default()
        })
        .unwrap()
        .code;
    let joined = hub
        .store
        .redeem_invite(&code, JoinClient::default())
        .unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let post = |url: &str, token: Option<&str>, body: Value| -> (u16, Value) {
        rt.block_on(async {
            let mut req = reqwest::Client::new().post(url).json(&body);
            if let Some(t) = token {
                req = req.bearer_auth(t);
            }
            let r = req.send().await.unwrap();
            let status = r.status().as_u16();
            (status, r.json().await.unwrap_or(Value::Null))
        })
    };
    let hb = format!("{}/agent/heartbeat", hub.url);
    assert_eq!(post(&hb, None, json!({})).0, 401, "no token");
    assert_eq!(
        post(&hb, Some("ano_bogus_x"), json!({})).0,
        401,
        "bad token"
    );
    for phase in [
        json!({}),
        json!({"phase": "start"}),
        json!({"phase": "beat"}),
    ] {
        let (status, body) = post(&hb, Some(&joined.token), phase.clone());
        assert_eq!(status, 200, "{phase} -> {body}");
    }
    let (status, body) = post(&hb, Some(&joined.token), json!({"phase": "nap"}));
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "invalid");
    let (status, _) = post(&hb, Some(&joined.token), json!({"phase": "end"}));
    assert_eq!(status, 200);

    // On a server without --auth there is no token agent to act as: 401, not a panic.
    let open = node("open", false, false);
    assert_eq!(
        post(&format!("{}/agent/heartbeat", open.url), None, json!({})).0,
        401
    );
    assert_eq!(
        post(&format!("{}/agent/leave", open.url), None, json!({})).0,
        401
    );
}

fn persistent_default() -> NewAgentInvite {
    NewAgentInvite {
        agent: String::new(),
        program: Program::Other,
        project_globs: vec![],
        lane: None,
        expires_in_seconds: None,
        max_uses: None,
        ephemeral: false,
        idle_hours: None,
        token_ttl_seconds: None,
    }
}

// ---------------------------------------------------------------------------------------
// join::load_profile (the profile `another join` saves)

#[test]
fn a_missing_or_corrupt_remote_profile_is_a_clean_error() {
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        Command::new(bin())
            .args(args)
            .env("ANOTHER_HOME", dir.path())
            .env_remove("ANOTHER_AGENT")
            .output()
            .unwrap()
    };
    let missing = run(&["remote", "status"]);
    assert!(!missing.status.success());
    let err = String::from_utf8_lossy(&missing.stderr);
    assert!(err.contains("join"), "points at `another join`: {err}");
    assert!(another::join::load_profile(dir.path()).is_err());

    for garbage in ["", "{", "[]", "{\"hub\": 3}", "\u{0}\u{1}"] {
        std::fs::write(dir.path().join("remote.json"), garbage).unwrap();
        let out = run(&["remote", "status"]);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{garbage:?}");
        assert!(!err.contains("panicked"), "{garbage:?}: {err}");
        assert!(
            another::join::load_profile(dir.path()).is_err(),
            "{garbage:?}"
        );
    }
}

// ---------------------------------------------------------------------------------------
// hooks::run_fail_open (hooks never block the client)

#[test]
fn hooks_fail_open_when_the_store_cannot_be_opened() {
    let dir = tempfile::tempdir().unwrap();
    // ANOTHER_HOME is a regular file: the store can't be opened.
    let home = dir.path().join("not-a-dir");
    std::fs::write(&home, "x").unwrap();
    let hook = |event: &str, client: &str| {
        let mut child = Command::new(bin())
            .args(["hook", "run", event, "--client", client])
            .env("ANOTHER_HOME", &home)
            .env("HOME", dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(br#"{"session_id": "s1", "cwd": "/tmp", "prompt": "hi"}"#)
            .unwrap();
        child.wait_with_output().unwrap()
    };
    for (event, client) in [
        ("session-start", "claude"),
        ("prompt", "codex"),
        ("stop", "grok"),
        ("pre-edit", "claude"),
    ] {
        let out = hook(event, client);
        assert!(out.status.success(), "{event}/{client} exits 0");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "",
            "{event}/{client} prints nothing that could block"
        );
    }
    // Cursor's prompt hook must answer, or Cursor waits: it gets {"continue": true}.
    let out = hook("prompt", "cursor");
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v, json!({ "continue": true }));
}

// ---------------------------------------------------------------------------------------
// hooks::session_digest

#[test]
fn the_session_digest_flags_importance_and_caps_requests_at_five() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.ensure_project("t", "/work/shop").unwrap();
    agent(&store, "claude", Program::Claude, Some("/work/shop"));
    agent(&store, "codex", Program::Codex, Some("/work/shop"));
    let send = |subject: &str, importance: Importance, request: bool| {
        store
            .send_message(SendMessage {
                from: "codex@shop".into(),
                to: vec!["claude@shop".into()],
                subject: subject.into(),
                body: "body text never appears in the digest".into(),
                importance,
                request: request.then(NewRequest::default),
                ..Default::default()
            })
            .unwrap()
    };
    send("Prod is down", Importance::Urgent, false);
    send("Review soon", Importance::High, false);
    send("Whenever", Importance::Low, false);
    let digest = another::hooks::session_digest(&store, "claude@shop").unwrap();
    assert!(
        digest.contains("[urgent] from codex@shop: \"Prod is down\""),
        "{digest}"
    );
    assert!(
        digest.contains("[high] from codex@shop: \"Review soon\""),
        "{digest}"
    );
    assert!(digest.contains("from codex@shop: \"Whenever\""), "{digest}");
    assert!(!digest.contains("[low]"), "{digest}");
    assert!(!digest.contains("body text"), "no bodies: {digest}");

    for i in 0..7 {
        send(&format!("Task {i}"), Importance::Normal, true);
    }
    // One request has an owner.
    let reqs = store.requests(Default::default()).unwrap();
    store
        .update_request(RequestUpdate {
            agent: "claude@shop".into(),
            request_id: reqs[0].request_id.clone(),
            action: RequestAction::Claim,
            note: None,
            due_in_seconds: None,
        })
        .unwrap();
    let digest = another::hooks::session_digest(&store, "claude@shop").unwrap();
    assert!(
        digest.contains("10 unread (2 urgent)"),
        "header counts all: {digest}"
    );
    assert!(
        digest.contains("7 open request(s)"),
        "header counts all: {digest}"
    );
    assert_eq!(
        digest.matches("\n- request ").count(),
        5,
        "five requests listed: {digest}"
    );
    assert!(digest.contains(", owner claude@shop)"), "{digest}");
    let listed = digest.matches("\n- from ").count() + digest.matches("\n- [").count();
    assert_eq!(listed, 5, "five messages listed: {digest}");
    // BUGS-FOUND.md #2: the five listed are the newest, whatever their importance, so the
    // urgent message drops out of the digest once five newer normal ones arrive (the header
    // still says "1 urgent").
    assert!(!digest.contains("Prod is down"), "{digest}");
    // An unknown agent is an error (the hook then fails open), not an empty digest.
    assert!(another::hooks::session_digest(&store, "nobody").is_err());
}

// =======================================================================================
// Second coverage pass: answers to mirrored decisions, the pre-edit and stop hooks, and
// `another join` against malformed input.

// ---------------------------------------------------------------------------------------
// Store::answer_decision on a mirror (another node's decision, relayed from here)

#[test]
fn mirrored_answers_are_checked_before_anything_is_sent() {
    let hub = node("hub", true, true);
    let spoke = node("laptop", false, false);
    hub.store.ensure_project("t", "/work/shop").unwrap();
    hub.store.ensure_project("t", "/work/vault").unwrap();
    agent(&hub.store, "claude", Program::Claude, Some("/work/shop"));
    agent(&hub.store, "manager", Program::GrokBot, None);
    spoke.store.ensure_project("t", "/src/shop").unwrap();
    agent(&spoke.store, "codex", Program::Codex, Some("/src/shop"));
    let code = hub
        .store
        .create_invite(&hub.url, &[Perm::ReadFeed, Perm::SendMail], 3600)
        .unwrap();
    fed::join(&spoke.store, &code).unwrap();
    let ask = |q: &str, allow_free_text: bool| {
        spoke
            .store
            .ask_decision(
                NewDecision {
                    agent: "codex@shop".into(),
                    question: q.into(),
                    options: options(),
                    allow_free_text,
                    ..Default::default()
                },
                None,
            )
            .unwrap()
            .decision_id
    };
    let strict = ask("Options only?", false);
    let by_number = ask("Answer by number?", false);
    let free = ask("Free text too?", true);
    let r = fed::sync(&spoke.store).unwrap();
    assert!(r.push_rejected.is_empty(), "{:?}", r.push_rejected);

    let answer = |id: &str,
                  value: Option<&str>,
                  text: Option<&str>,
                  relayed: Option<&str>,
                  by: Option<&str>| {
        hub.store.answer_decision(
            AnswerDecision {
                decision: id.into(),
                value: value.map(Into::into),
                text: text.map(Into::into),
                answered_by: by.map(Into::into),
                relayed_by: relayed.map(Into::into),
            },
            None,
        )
    };
    let m = Some("manager");
    let refused = |r: another_core::Result<another_core::Decision>, want: &str| {
        let e = r.unwrap_err().to_string();
        assert!(e.contains(want), "{want}: {e}");
    };
    refused(
        answer(&strict, Some("maybe"), None, m, None),
        "not an option",
    );
    refused(answer(&strict, Some("0"), None, m, None), "not an option");
    refused(answer(&strict, Some("3"), None, m, None), "not an option");
    refused(answer(&strict, None, None, m, None), "give an option value");
    refused(
        answer(&strict, Some(" "), Some(" "), m, None),
        "give an option",
    );
    refused(answer(&strict, None, Some("why not"), m, None), "free text");
    let long = "n".repeat(another_core::MAX_OPTION_NOTE + 1);
    assert!(answer(&strict, Some("yes"), Some(&long), m, None).is_err());
    // Someone here has to carry it; a person who isn't registered here doesn't count.
    refused(
        answer(&strict, Some("yes"), None, None, None),
        "relaying agent",
    );
    refused(
        answer(&strict, Some("yes"), None, None, Some("stranger")),
        "relaying agent",
    );
    assert!(answer(&strict, Some("yes"), None, Some("nobody"), None).is_err());
    // A token that can't see the mirror gets the same answer as for an unknown id.
    let vault = {
        let t = hub
            .store
            .create_token("claude@shop", &["/work/vault".into()], None, None)
            .unwrap();
        hub.store.verify_token(&t.secret).unwrap()
    };
    let scoped = |id: &str| {
        hub.store
            .answer_decision(
                AnswerDecision {
                    decision: id.into(),
                    value: Some("yes".into()),
                    relayed_by: Some("manager".into()),
                    ..Default::default()
                },
                Some(&vault),
            )
            .unwrap_err()
            .code()
            .to_string()
    };
    assert_eq!(scoped(&strict), "not_found");
    assert_eq!(scoped("01ZZZZZZZZZZZZZZZZZZZZZZZZ"), "not_found");
    assert_eq!(
        hub.store.decision(&strict, None).unwrap().status,
        DecisionStatus::Open,
        "nothing refused above touched the mirror"
    );

    // Case doesn't matter, and a note may go with an option.
    let sent = answer(&strict, Some("YES"), Some("ok by me"), m, Some("jacob")).unwrap();
    assert_eq!(sent.status, DecisionStatus::AnswerSent);
    refused(answer(&strict, Some("no"), None, m, None), "already");
    // A number picks an option; free text goes through where it's allowed.
    let sent = answer(&by_number, Some("2"), None, m, None).unwrap();
    assert_eq!(sent.status, DecisionStatus::AnswerSent);
    let sent = answer(&free, None, Some("half of it"), m, None).unwrap();
    assert_eq!(sent.status, DecisionStatus::AnswerSent);

    // The asker's node gets all three, and a second sync changes nothing.
    fed::sync(&spoke.store).unwrap();
    fed::sync(&spoke.store).unwrap();
    let got = |id: &str| spoke.store.decision(id, None).unwrap();
    assert_eq!(got(&strict).status, DecisionStatus::Answered);
    assert_eq!(got(&by_number).answer.unwrap().value.as_deref(), Some("no"));
    assert_eq!(
        got(&free).answer.unwrap().text.as_deref(),
        Some("half of it")
    );
}

// ---------------------------------------------------------------------------------------
// hooks::run (pre-edit) and hooks::lane_auto_report (stop), through `another hook run`

struct Repo {
    _dir: tempfile::TempDir,
    home: std::path::PathBuf,
    user: std::path::PathBuf,
    repo: std::path::PathBuf,
}

fn repo() -> Repo {
    let dir = tempfile::tempdir().unwrap();
    let (home, user, repo) = (
        dir.path().join("h"),
        dir.path().join("u"),
        dir.path().join("shop"),
    );
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(&user).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    Repo {
        _dir: dir,
        home,
        user,
        repo,
    }
}

impl Repo {
    fn store(&self) -> Store {
        Store::open(&self.home).unwrap()
    }

    /// Run a hook with `payload` on stdin; returns stdout. Hooks always exit 0.
    fn hook(&self, event: &str, client: &str, envs: &[(&str, &str)], payload: Value) -> String {
        let mut c = Command::new(bin());
        c.args(["hook", "run", event, "--client", client])
            .env("ANOTHER_HOME", &self.home)
            .env("HOME", &self.user)
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_LANE")
            .env_remove("GROK_HOOK_EVENT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in envs {
            c.env(k, v);
        }
        let mut child = c.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{event} exits 0");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn reports(&self, lane: &str) -> Vec<String> {
        self.store()
            .lane(lane, None)
            .unwrap()
            .reports
            .into_iter()
            .map(|r| r.summary)
            .collect()
    }
}

fn shop_with_lane(r: &Repo) -> String {
    let path = r.repo.to_str().unwrap().to_string();
    let s = r.store();
    s.ensure_project("t", &path).unwrap();
    agent(&s, "manager", Program::GrokBot, None);
    agent(&s, "codex", Program::Codex, Some(&path));
    agent(&s, "claude", Program::Claude, Some(&path));
    s.create_lane(
        NewLane {
            agent: "manager".into(),
            name: "search".into(),
            project: path.clone(),
            lead: "codex@shop".into(),
            charter: String::new(),
            status: None,
        },
        None,
    )
    .unwrap();
    path
}

#[test]
fn a_pre_edit_denial_counts_the_other_files_and_keeps_its_warning() {
    let r = repo();
    let path = shop_with_lane(&r);
    r.store()
        .acquire_holds(AcquireHolds {
            agent: "claude@shop".into(),
            project: path.clone(),
            paths: vec!["src/a.rs".into(), "src/b.rs".into(), "src/c.rs".into()],
            ttl_seconds: Some(3600),
            exclusive: true,
            reason: Some("refactor".into()),
            lane: None,
        })
        .unwrap();
    let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-a\n+b\n\
                 *** Update File: src/b.rs\n@@\n-a\n+b\n\
                 *** Update File: src/c.rs\n@@\n-a\n+b\n*** End Patch\n";
    let payload = json!({ "cwd": r.repo, "hook_event_name": "PreToolUse",
                          "tool_name": "apply_patch", "tool_input": { "patch": patch } });
    let out = r.hook(
        "pre-edit",
        "codex",
        &[("ANOTHER_AGENT", "codex@shop")],
        payload.clone(),
    );
    let v: Value = serde_json::from_str(&out).expect("deny JSON");
    let reason = v["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(reason.contains(" 2 more file(s) conflict."), "{reason}");
    assert!(reason.contains("for \"refactor\""), "{reason}");
    assert_eq!(v.get("systemMessage"), None, "{v}");

    // With a bad $ANOTHER_LANE the edit is still denied and the warning stays visible.
    let out = r.hook(
        "pre-edit",
        "codex",
        &[
            ("ANOTHER_AGENT", "codex@shop"),
            ("ANOTHER_LANE", "no-such-lane"),
        ],
        payload,
    );
    let v: Value = serde_json::from_str(&out).expect("deny JSON");
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(
        v["systemMessage"]
            .as_str()
            .unwrap()
            .contains("no-such-lane"),
        "{v}"
    );

    // An edit with no files to hold, or a payload that isn't JSON at all: never a block.
    let quiet = r.hook(
        "pre-edit",
        "codex",
        &[("ANOTHER_AGENT", "codex@shop")],
        json!({ "cwd": r.repo, "tool_name": "apply_patch", "tool_input": {} }),
    );
    assert!(!quiet.contains("deny"), "{quiet}");
    let not_an_object = r.hook(
        "pre-edit",
        "codex",
        &[("ANOTHER_AGENT", "codex@shop")],
        json!("nonsense"),
    );
    assert!(!not_an_object.contains("deny"), "{not_an_object}");
}

#[test]
fn the_stop_hook_reports_once_per_interval_and_only_to_an_open_lane() {
    let r = repo();
    shop_with_lane(&r);
    let state = r.home.join("hooks").join("state");
    let stop = |envs: &[(&str, &str)], message: Option<&str>| {
        let mut p = json!({ "cwd": r.repo, "session_id": "s" });
        if let Some(m) = message {
            p["last_assistant_message"] = json!(m);
        }
        let out = r.hook("stop", "codex", envs, p);
        assert!(out.is_empty(), "stop prints nothing: {out}");
    };
    let clear = || {
        if state.exists() {
            for f in std::fs::read_dir(&state).unwrap() {
                let f = f.unwrap().path();
                if f.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("lane-")
                {
                    std::fs::remove_file(f).unwrap();
                }
            }
        }
    };

    // The client gave no summary, or only whitespace: a stock line, not an empty report.
    stop(&[], None);
    assert_eq!(
        r.reports("search"),
        ["Turn finished (no summary from the client)."]
    );
    // A double run within the interval adds nothing, whatever it says.
    stop(&[], Some("second"));
    assert_eq!(r.reports("search").len(), 1);

    // An unreadable interval marker counts as never reported, not as a failure.
    let marker = std::fs::read_dir(&state)
        .unwrap()
        .map(|f| f.unwrap().path())
        .find(|f| {
            f.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("lane-")
        })
        .expect("the interval marker");
    std::fs::write(&marker, "not a number").unwrap();
    stop(&[], Some("   "));
    assert_eq!(r.reports("search").len(), 2);

    // Named by $ANOTHER_LANE, with secrets taken out of what the client said.
    clear();
    let secret = format!("ano_{}", "a".repeat(40));
    stop(
        &[("ANOTHER_LANE", "search")],
        Some(&format!("used {secret} here")),
    );
    let reports = r.reports("search");
    assert_eq!(reports.len(), 3, "{reports:?}");
    assert!(reports.iter().all(|s| !s.contains(&secret)), "{reports:?}");

    // $ANOTHER_LANE naming nothing: the hook fails open and reports nowhere.
    clear();
    stop(&[("ANOTHER_LANE", "no-such-lane")], Some("lost"));
    stop(&[("ANOTHER_LANE", "   ")], Some("blank is unset"));
    assert_eq!(r.reports("search").len(), 4);

    // A closed lane takes no more reports.
    clear();
    r.store()
        .close_lane("manager", "search", true, None, None)
        .unwrap();
    let closed = r.reports("search").len();
    stop(&[("ANOTHER_LANE", "search")], Some("after the end"));
    stop(&[], Some("after the end"));
    assert_eq!(r.reports("search").len(), closed);
}

// ---------------------------------------------------------------------------------------
// join::join and POST /join against malformed input

fn join_cli(hub: &str, code: &str, home: &std::path::Path, user: &std::path::Path) -> Output {
    Command::new(bin())
        .args(["--json", "join", hub, code, "--write-mcp", "codex"])
        .env("ANOTHER_HOME", home)
        .env("HOME", user)
        .env_remove("ANOTHER_AGENT")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn the_join_route_refuses_malformed_bodies_without_using_the_invite() {
    let hub = node("hub", true, false);
    hub.store.ensure_project("t", "/work/shop").unwrap();
    hub.store
        .create_agent_invite(NewAgentInvite {
            agent: "cloud".into(),
            program: Program::Codex,
            project_globs: vec!["/work/shop".into()],
            ..persistent_default()
        })
        .unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let post = |body: Vec<u8>| -> (u16, Value) {
        rt.block_on(async {
            let r = reqwest::Client::new()
                .post(format!("{}/join", hub.url))
                .body(body)
                .send()
                .await
                .unwrap();
            let status = r.status().as_u16();
            (status, r.json().await.unwrap_or(Value::Null))
        })
    };
    for body in [
        Vec::new(),
        b"not json".to_vec(),
        b"[]".to_vec(),
        b"{\"code\": 7}".to_vec(),
        vec![0xff, 0xfe],
        // Past the 64 KiB limit, with a valid shape.
        format!("{{\"code\": \"{}\"}}", "x".repeat(70 * 1024)).into_bytes(),
    ] {
        let (status, v) = post(body);
        assert_eq!(status, 400, "{v}");
        assert_eq!(v["error"]["code"], "invalid", "{v}");
    }
    // Well formed but wrong: refused, and no more telling than that.
    let (status, wrong) = post(b"{\"code\": \"anj_nope_nope\"}".to_vec());
    assert_eq!(status, 403, "{wrong}");
    assert_eq!(hub.store.agent_invites().unwrap()[0].uses, 0);
}

#[test]
fn join_checks_the_hub_url_and_leaves_an_existing_codex_config_alone() {
    let hub = node("hub", true, false);
    hub.store.ensure_project("t", "/work/shop").unwrap();
    let invite = |name: &str, uses: u32| {
        hub.store
            .create_agent_invite(NewAgentInvite {
                agent: name.into(),
                program: Program::Codex,
                project_globs: vec!["/work/shop".into()],
                max_uses: Some(uses),
                ..persistent_default()
            })
            .unwrap()
            .code
    };
    let dir = tempfile::tempdir().unwrap();
    let (home, user) = (dir.path().join("h"), dir.path().join("u"));
    std::fs::create_dir_all(user.join(".codex")).unwrap();

    // Not http(s): refused before anything is sent, so the invite is still good.
    let code = invite("cloud", 1);
    let out = join_cli("ftp://hub.example", &code, &home, &user);
    assert!(!out.status.success());
    // With --json the error is JSON on stdout.
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("must start with https://"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(!home.join("remote.json").exists());

    // An existing config with no final newline gets one before the new table.
    let cfg = user.join(".codex/config.toml");
    std::fs::write(&cfg, "model = \"x\"").unwrap();
    let out = join_cli(&format!("{}/", hub.url), &code, &home, &user);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = std::fs::read_to_string(&cfg).unwrap();
    assert!(text.starts_with("model = \"x\"\n\n"), "{text}");
    assert!(text.contains("[mcp_servers.another]"), "{text}");
    assert!(home.join("remote.json").exists());

    // Redeeming the same code again is refused and changes nothing here.
    let profile = std::fs::read(home.join("remote.json")).unwrap();
    let again = join_cli(&hub.url, &code, &home, &user);
    assert!(!again.status.success());
    assert_eq!(std::fs::read(home.join("remote.json")).unwrap(), profile);
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), text);

    // A later join with the table already there leaves the file as it is and says so.
    let second = invite("cloud2", 1);
    let out = join_cli(&hub.url, &second, &home, &user);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(said.contains("left unchanged"), "{said}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), text);
}
