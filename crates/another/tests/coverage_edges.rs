//! Edge cases for the least-covered public functions found by the v0.1.0 coverage pass
//! (`cargo llvm-cov`): mirrored decisions by id, the remote heartbeat route, the saved
//! remote profile, fail-open hooks and the session digest. Tests only.

use std::net::SocketAddr;
use std::process::{Command, Stdio};

use another::fed;
use another::serve::{http_router, HttpOptions};
use another_core::{
    DecisionOption, Importance, JoinClient, NewAgentInvite, NewDecision, NewRequest, Perm, Program,
    RegisterAgent, RequestAction, RequestUpdate, SendMessage, Store,
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
