//! Group work queues (D77) through the real binary, separate processes, MCP and the remote
//! gate.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use another::mcp::AnotherMcp;
use another_core::{
    NewRequest, NextRequest, Priority, Program, RegisterAgent, RequestAction, RequestUpdate,
    SendMessage, Store,
};
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Map, Value};

struct Env {
    _dir: tempfile::TempDir,
    home: PathBuf,
    another_home: PathBuf,
    shop: PathBuf,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let shop = root.join("shop");
        std::fs::create_dir_all(&shop).unwrap();
        let env = Env {
            home: root.join("user"),
            another_home: root.join("another-home"),
            _dir: dir,
            shop,
        };
        std::fs::create_dir_all(&env.home).unwrap();
        env.ok(&["init"]);
        for (name, program) in [("lead", "claude"), ("alice", "claude"), ("bob", "codex")] {
            env.ok(&[
                "agent",
                "register",
                name,
                "--program",
                program,
                "--project",
                env.shop.to_str().unwrap(),
            ]);
        }
        env
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_another"));
        c.args(args)
            .env("HOME", &self.home)
            .env("ANOTHER_HOME", &self.another_home)
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_LOG")
            .env_remove("HERDR_PANE_ID")
            .env_remove("TMUX_PANE")
            .env_remove("CMUX_SURFACE_ID")
            .env_remove("TERM_PROGRAM");
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).stdin(Stdio::null()).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "another {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        serde_json::from_str(&self.ok(&all)).unwrap()
    }

    fn queue(&self, subject: &str, priority: &str) -> String {
        self.json(&[
            "send",
            "--from",
            "lead@shop",
            "--to",
            "group:reviewers",
            "--priority",
            priority,
            "--subject",
            subject,
            "--body",
            "see the PR",
        ])["request_id"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

#[test]
fn cli_creates_a_group_queues_work_and_hands_it_out_once() {
    let env = Env::new();
    let shop = env.shop.to_str().unwrap();
    let created = env.ok(&[
        "group",
        "create",
        "reviewers",
        "--agent",
        "alice@shop",
        "--project",
        shop,
    ]);
    assert!(
        created.contains("created group reviewers@shop"),
        "{created}"
    );
    env.ok(&["group", "join", "reviewers", "--agent", "bob@shop"]);

    let low = env.queue("Tidy the changelog", "low");
    let high = env.queue("Review #143", "high");
    // Plain mail to a group is refused: groups queue work, they do not broadcast.
    let plain = env.run(&[
        "send",
        "--from",
        "lead@shop",
        "--to",
        "group:reviewers",
        "--subject",
        "hi",
    ]);
    assert!(!plain.status.success());
    assert!(String::from_utf8_lossy(&plain.stderr).contains("not plain mail"));

    let listed = env.ok(&["group", "list"]);
    assert!(
        listed.contains("reviewers@shop  members: alice@shop, bob@shop"),
        "{listed}"
    );
    assert!(
        listed.contains("2 open (urgent 0, high 1, normal 0, low 1)"),
        "{listed}"
    );

    let first = env.json(&["request", "next", "--agent", "bob@shop"]);
    assert_eq!(first["request_id"], high, "high before low");
    assert_eq!(first["owner"], "bob@shop");
    assert_eq!(first["group"], "reviewers@shop");
    assert_eq!(first["priority"], "high");
    let second = env.json(&["request", "next", "--agent", "alice@shop"]);
    assert_eq!(second["request_id"], low);
    assert_eq!(
        env.ok(&["request", "next", "--agent", "alice@shop"]).trim(),
        "nothing queued for you"
    );
    assert_eq!(
        env.json(&["request", "next", "--agent", "alice@shop"]),
        Value::Null
    );

    let in_queue = env.json(&["requests", "--group", "reviewers@shop"]);
    assert_eq!(in_queue.as_array().unwrap().len(), 2);
    let text = env.ok(&["requests", "--group", "reviewers@shop"]);
    assert!(text.contains("high queue reviewers@shop"), "{text}");

    env.ok(&["request", "complete", &high, "--agent", "bob@shop"]);
    let lead = env.ok(&["inbox", "--agent", "lead@shop"]);
    assert!(lead.contains("Done: Review #143"), "{lead}");

    // Wrong shapes are refused with a pointer to the right one.
    let with_id = env.run(&["request", "next", &low, "--agent", "alice@shop"]);
    assert!(!with_id.status.success());
    let no_id = env.run(&["request", "claim", "--agent", "alice@shop"]);
    assert!(!no_id.status.success());
    let stray = env.run(&[
        "request",
        "release",
        &low,
        "--agent",
        "alice@shop",
        "--group",
        "reviewers",
    ]);
    assert!(!stray.status.success());
}

/// Separate `another` processes race for the same queue: every item is handed out once.
#[test]
fn separate_processes_never_get_the_same_item() {
    let env = Env::new();
    let shop = env.shop.to_str().unwrap().to_string();
    env.ok(&[
        "group",
        "create",
        "reviewers",
        "--agent",
        "alice@shop",
        "--project",
        &shop,
    ]);
    env.ok(&["group", "join", "reviewers", "--agent", "bob@shop"]);
    env.ok(&["group", "join", "reviewers", "--agent", "lead@shop"]);
    let total = 24;
    let ids: HashSet<String> = (0..total)
        .map(|i| env.queue(&format!("item {i}"), "normal"))
        .collect();
    let workers: Vec<_> = ["alice@shop", "bob@shop", "lead@shop", "alice@shop"]
        .into_iter()
        .map(|agent| {
            let bin = env!("CARGO_BIN_EXE_another");
            let (home, another_home) = (env.home.clone(), env.another_home.clone());
            std::thread::spawn(move || {
                let mut mine = Vec::new();
                loop {
                    let out = Command::new(bin)
                        .args(["--json", "request", "next", "--agent", agent])
                        .env("HOME", &home)
                        .env("ANOTHER_HOME", &another_home)
                        .env_remove("ANOTHER_AGENT")
                        .stdin(Stdio::null())
                        .output()
                        .unwrap();
                    assert!(
                        out.status.success(),
                        "{}",
                        String::from_utf8_lossy(&out.stderr)
                    );
                    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
                    match v["request_id"].as_str() {
                        Some(id) => mine.push(id.to_string()),
                        None => break,
                    }
                }
                mine
            })
        })
        .collect();
    let mut seen = Vec::new();
    for w in workers {
        seen.extend(w.join().unwrap());
    }
    let unique: HashSet<String> = seen.iter().cloned().collect();
    assert_eq!(seen.len(), total, "{seen:?}");
    assert_eq!(unique, ids);
}

type Client = RunningService<RoleClient, ClientConfig>;

async fn in_process(store: Store) -> Client {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let server = AnotherMcp::new(store).serve(server_io).await.unwrap();
        let _ = server.waiting().await;
    });
    ClientConfig::default().serve(client_io).await.unwrap()
}

async fn call(client: &Client, tool: &str, args: Value) -> CallToolResult {
    client
        .call_tool(
            CallToolRequestParams::new(tool.to_string())
                .with_arguments(args.as_object().unwrap().clone()),
        )
        .await
        .unwrap()
}

fn text(r: &CallToolResult) -> String {
    serde_json::to_string(r).unwrap()
}

fn local_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let shop = dir.path().join("shop");
    std::fs::create_dir_all(&shop).unwrap();
    let store = Store::open(&dir.path().join("home")).unwrap();
    for name in ["lead", "bob"] {
        store
            .register_agent(RegisterAgent {
                name: name.into(),
                program: Program::Claude,
                project: Some(shop.to_string_lossy().into_owned()),
                model: None,
                description: None,
            })
            .unwrap();
    }
    store
        .create_group("bob", &shop.to_string_lossy(), "reviewers")
        .unwrap();
    (dir, store)
}

#[tokio::test]
async fn mcp_queues_and_pulls_work() {
    let (_dir, store) = local_store();
    let client = in_process(store).await;
    let sent = call(
        &client,
        "send_message",
        json!({"from": "lead", "to": ["group:reviewers"], "subject": "Review", "body": "b",
               "create_request": true, "priority": "urgent"}),
    )
    .await;
    assert_ne!(sent.is_error, Some(true), "{}", text(&sent));
    let next = call(
        &client,
        "claim_request",
        json!({"agent": "bob", "action": "next"}),
    )
    .await;
    assert_ne!(next.is_error, Some(true), "{}", text(&next));
    let body = text(&next);
    assert!(body.contains("\"owner\":\"bob@shop\""), "{body}");
    assert!(body.contains("\"priority\":\"urgent\""), "{body}");
    let empty = call(
        &client,
        "claim_request",
        json!({"agent": "bob", "action": "next"}),
    )
    .await;
    assert_eq!(empty.is_error, Some(true));
    assert!(text(&empty).contains("nothing queued"), "{}", text(&empty));
    let missing_id = call(
        &client,
        "claim_request",
        json!({"agent": "bob", "action": "release"}),
    )
    .await;
    assert_eq!(missing_id.is_error, Some(true));
    client.cancel().await.unwrap();
}

#[test]
fn remote_callers_get_no_group_operations() {
    let (_dir, store) = local_store();
    let rid = store
        .send_message(SendMessage {
            from: "lead".into(),
            to: vec!["group:reviewers".into()],
            subject: "Review".into(),
            body: "b".into(),
            request: Some(NewRequest {
                priority: Priority::High,
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap()
        .request_id
        .unwrap();
    let token = store
        .create_token("bob", &["**".to_string()], None, None)
        .unwrap();
    let scope = store.verify_token(&token.secret).unwrap();
    let mut send: Map<String, Value> = json!({"from": "bob", "to": ["group:reviewers"],
        "subject": "s", "body": "b", "create_request": true})
    .as_object()
    .unwrap()
    .clone();
    let refused = another::remote::authorize(&store, &scope, "send_message", &mut send)
        .expect_err("no remote group send");
    assert!(refused.contains("local-only"), "{refused}");
    let mut next: Map<String, Value> = json!({"agent": "bob", "action": "next"})
        .as_object()
        .unwrap()
        .clone();
    let refused = another::remote::authorize(&store, &scope, "claim_request", &mut next)
        .expect_err("no remote pull");
    assert!(refused.contains("local-only"), "{refused}");
    // By id, through the core's scoped path: refused before anything changes.
    for action in [
        RequestAction::Claim,
        RequestAction::Complete,
        RequestAction::Cancel,
    ] {
        let err = store
            .update_request_scoped(
                RequestUpdate {
                    agent: "bob".into(),
                    request_id: rid.clone(),
                    action,
                    note: None,
                    due_in_seconds: None,
                },
                Some(&scope),
            )
            .unwrap_err();
        assert!(matches!(err, another_core::Error::Forbidden(_)), "{err:?}");
    }
    assert_eq!(store.request(&rid).unwrap().owner, None);

    // An item bob claimed locally is left out of his remote inbox and catch-up, over a
    // real authenticated HTTP server with his bearer token.
    store
        .request_next(NextRequest {
            agent: "bob".into(),
            ..Default::default()
        })
        .unwrap()
        .unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let local = in_process(store.clone()).await;
        let mine = call(&local, "fetch_inbox", json!({"agent": "bob"})).await;
        assert!(
            text(&mine).contains(&rid),
            "local callers see their queue item"
        );
        local.cancel().await.unwrap();

        let ct = tokio_util::sync::CancellationToken::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let opts = another::serve::HttpOptions {
            auth: true,
            bound_port: addr.port(),
            ..Default::default()
        };
        let app = another::serve::http_router(store.clone(), ct.clone(), &opts);
        let stop = ct.clone();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(async move { stop.cancelled_owned().await })
            .await
            .unwrap();
        });
        let transport = rmcp::transport::StreamableHttpClientTransport::from_config(
            rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(
                format!("http://{addr}/mcp"),
            )
            .auth_header(token.secret.clone()),
        );
        let remote = ClientConfig::default().serve(transport).await.unwrap();
        let inbox = call(&remote, "fetch_inbox", json!({"agent": "bob"})).await;
        assert_ne!(inbox.is_error, Some(true), "{}", text(&inbox));
        assert!(!text(&inbox).contains(&rid), "{}", text(&inbox));
        let news = call(&remote, "whats_new", json!({"agent": "bob"})).await;
        assert_ne!(news.is_error, Some(true), "{}", text(&news));
        assert!(!text(&news).contains(&rid), "{}", text(&news));
        assert!(
            text(&news).contains("Nothing is waiting"),
            "next counts only what is shown: {}",
            text(&news)
        );
        let feed = call(&remote, "activity_feed", json!({})).await;
        assert_ne!(feed.is_error, Some(true), "{}", text(&feed));
        assert!(!text(&feed).contains(&rid), "{}", text(&feed));
        assert!(!text(&feed).contains("group."), "{}", text(&feed));
        let http = reqwest::Client::new()
            .get(format!("http://{addr}/activity?since=1h"))
            .bearer_auth(&token.secret)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(http.contains("\"items\""), "{http}");
        assert!(!http.contains(&rid), "{http}");
        let pull = call(
            &remote,
            "claim_request",
            json!({"agent": "bob", "action": "next"}),
        )
        .await;
        assert_eq!(pull.is_error, Some(true), "{}", text(&pull));
        let complete = call(
            &remote,
            "claim_request",
            json!({"agent": "bob", "request_id": rid, "action": "complete"}),
        )
        .await;
        assert_eq!(complete.is_error, Some(true), "{}", text(&complete));
        remote.cancel().await.unwrap();
        ct.cancel();
    });
    assert_eq!(
        store.request(&rid).unwrap().status,
        another_core::RequestStatus::Claimed,
        "nothing changed remotely"
    );
}

/// An authenticated remote MCP client for `secret` over a real loopback HTTP server.
async fn remote_client(
    store: &Store,
    secret: &str,
) -> (Client, tokio_util::sync::CancellationToken) {
    let ct = tokio_util::sync::CancellationToken::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let opts = another::serve::HttpOptions {
        auth: true,
        bound_port: addr.port(),
        ..Default::default()
    };
    let app = another::serve::http_router(store.clone(), ct.clone(), &opts);
    let stop = ct.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move { stop.cancelled_owned().await })
        .await
        .unwrap();
    });
    let transport = rmcp::transport::StreamableHttpClientTransport::from_config(
        rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(
            format!("http://{addr}/mcp"),
        )
        .auth_header(secret.to_string()),
    );
    (ClientConfig::default().serve(transport).await.unwrap(), ct)
}

/// REVIEW-IMPL P2-1 and P2-2: once a queue item's thread has local participants (its claimer,
/// or a direct recipient of a mixed send), no remote path reads it, replies in it, acts on its
/// mail or summarizes it. Ordinary direct threads are unchanged.
#[tokio::test]
async fn remote_callers_cannot_reach_a_queue_thread_by_any_path() {
    let (dir, store) = local_store();
    store
        .register_agent(RegisterAgent {
            name: "carol".into(),
            program: Program::Claude,
            project: Some(dir.path().join("shop").to_string_lossy().into_owned()),
            model: None,
            description: None,
        })
        .unwrap();
    // A queue item sent to the group and, directly, to carol.
    let item = store
        .send_message(SendMessage {
            from: "lead".into(),
            to: vec!["group:reviewers".into(), "carol".into()],
            subject: "private-queue-title".into(),
            body: "private-queue-body".into(),
            request: Some(NewRequest::default()),
            ..Default::default()
        })
        .unwrap();
    let claimed = store
        .request_next(NextRequest {
            agent: "bob".into(),
            ..Default::default()
        })
        .unwrap()
        .unwrap();
    // A direct thread, the control.
    let direct = store
        .send_message(SendMessage {
            from: "lead".into(),
            to: vec!["bob".into(), "carol".into()],
            subject: "direct-control".into(),
            body: "direct-body".into(),
            ..Default::default()
        })
        .unwrap();

    for who in ["bob", "carol"] {
        let token = store
            .create_token(who, &["**".to_string()], None, None)
            .unwrap();
        let (remote, ct) = remote_client(&store, &token.secret).await;
        let refused = |r: &CallToolResult, what: &str| {
            assert_eq!(r.is_error, Some(true), "{who} {what}: {}", text(r));
        };
        let hidden = |r: &CallToolResult, what: &str| {
            assert_ne!(r.is_error, Some(true), "{who} {what}: {}", text(r));
            let t = text(r);
            assert!(
                !t.contains("private-queue") && !t.contains(&claimed.request_id),
                "{who} {what} shows the queue item: {t}"
            );
        };
        hidden(
            &call(&remote, "threads", json!({"agent": who})).await,
            "thread list",
        );
        refused(
            &call(&remote, "threads", json!({"thread_id": item.thread_id})).await,
            "thread read",
        );
        refused(
            &call(
                &remote,
                "send_message",
                json!({"from": who, "reply_to": item.message_id, "body": "remote reply"}),
            )
            .await,
            "reply",
        );
        refused(
            &call(
                &remote,
                "acknowledge",
                json!({"agent": who, "message_ids": [item.message_id]}),
            )
            .await,
            "acknowledge",
        );
        hidden(
            &call(&remote, "fetch_inbox", json!({"agent": who})).await,
            "inbox",
        );
        hidden(
            &call(&remote, "whats_new", json!({"agent": who})).await,
            "whats_new",
        );
        hidden(
            &call(&remote, "activity_feed", json!({"limit": 200})).await,
            "activity feed",
        );
        hidden(
            &call(&remote, "activity_summarize", json!({})).await,
            "summary",
        );
        hidden(
            &call(&remote, "work_board", json!({"project": "shop"})).await,
            "board",
        );

        // The direct thread works remotely as before.
        let listed = call(&remote, "threads", json!({"agent": who})).await;
        assert!(
            text(&listed).contains("direct-control"),
            "{}",
            text(&listed)
        );
        let read = call(&remote, "threads", json!({"thread_id": direct.thread_id})).await;
        assert!(text(&read).contains("direct-body"), "{}", text(&read));
        let reply = call(
            &remote,
            "send_message",
            json!({"from": who, "reply_to": direct.message_id, "body": "fine"}),
        )
        .await;
        assert_ne!(reply.is_error, Some(true), "{}", text(&reply));

        remote.cancel().await.unwrap();
        ct.cancel();
    }
    // Locally, nothing is hidden.
    let local = in_process(store.clone()).await;
    let read = call(&local, "threads", json!({"thread_id": item.thread_id})).await;
    assert!(
        text(&read).contains("private-queue-body"),
        "{}",
        text(&read)
    );
    local.cancel().await.unwrap();
}

/// REVIEW-IMPL-R2 P2: the redacted dashboard (`--auth --dashboard`) is a read path for
/// whoever can reach the server, so it leaves queue items, their threads and their events
/// out of the snapshot and the activity search, as every other remote read does. A direct
/// request is the control. The local dashboard still shows everything.
#[tokio::test]
async fn the_redacted_dashboard_shows_no_queue_items() {
    let (_dir, store) = local_store();
    let item = store
        .send_message(SendMessage {
            from: "lead".into(),
            to: vec!["group:reviewers".into()],
            subject: "private-queue-title".into(),
            body: "b".into(),
            request: Some(NewRequest::default()),
            ..Default::default()
        })
        .unwrap();
    let claimed = store
        .request_next(NextRequest {
            agent: "bob".into(),
            ..Default::default()
        })
        .unwrap()
        .unwrap();
    store
        .send_message(SendMessage {
            from: "lead".into(),
            to: vec!["bob".into()],
            subject: "direct-control-request".into(),
            body: "b".into(),
            request: Some(NewRequest::default()),
            ..Default::default()
        })
        .unwrap();

    let get = |mode: another::dashboard::DashboardMode| {
        let store = store.clone();
        async move {
            let ct = tokio_util::sync::CancellationToken::new();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let opts = another::serve::HttpOptions {
                auth: mode == another::dashboard::DashboardMode::Redacted,
                bound_port: addr.port(),
                dashboard: mode,
                ..Default::default()
            };
            let app = another::serve::http_router(store, ct.clone(), &opts);
            let stop = ct.clone();
            tokio::spawn(async move {
                axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
                )
                .with_graceful_shutdown(async move { stop.cancelled_owned().await })
                .await
                .unwrap();
            });
            let http = reqwest::Client::new();
            let mut out = Vec::new();
            for path in ["/api/dash", "/api/dash/activity?limit=200"] {
                let resp = http
                    .get(format!("http://{addr}{path}"))
                    .send()
                    .await
                    .unwrap();
                assert!(resp.status().is_success(), "{path}: {}", resp.status());
                out.push(resp.text().await.unwrap());
            }
            ct.cancel();
            out
        }
    };

    let redacted = get(another::dashboard::DashboardMode::Redacted).await;
    for (path, text) in ["/api/dash", "/api/dash/activity"].iter().zip(&redacted) {
        assert!(
            !text.contains("private-queue-title")
                && !text.contains(&claimed.request_id)
                && !text.contains(&item.thread_id),
            "redacted {path} shows the queue item: {text}"
        );
    }
    assert!(
        redacted[0].contains("direct-control-request"),
        "the direct request is still shown: {}",
        redacted[0]
    );
    let local = get(another::dashboard::DashboardMode::Local).await;
    assert!(
        local[0].contains("private-queue-title"),
        "the local dashboard shows everything"
    );
}
