//! Push end to end: remote webhook subscriptions over authenticated MCP, signed deliveries
//! to a loopback receiver, `events_poll`, `GET /events/poll`, `GET /events/stream`, the
//! audit log, and the `another webhook` CLI. Everything runs on a temporary ANOTHER_HOME and
//! ephemeral loopback ports.

use std::net::SocketAddr;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use another::serve::{http_router, HttpOptions};
use another_core::{NewLane, Program, RegisterAgent, Store};
use axum::http::HeaderMap;
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

type Client = RunningService<RoleClient, ClientConfig>;
type Received = Arc<Mutex<Vec<(HeaderMap, String)>>>;

struct Server {
    _dir: tempfile::TempDir,
    store: Store,
    base: String,
    ct: CancellationToken,
}

async fn serve_on_loopback(app: axum::Router, ct: CancellationToken) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    assert_ne!(addr.port(), 7466);
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { ct.cancelled_owned().await })
        .await
        .unwrap();
    });
    addr
}

async fn start() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.ensure_project("t", "/work/shop").unwrap();
    store.ensure_project("t", "/work/vault").unwrap();
    for (name, project) in [
        ("manager", None),
        ("shopper", Some("/work/shop")),
        ("vaulter", Some("/work/vault")),
    ] {
        store
            .register_agent(RegisterAgent {
                name: name.into(),
                program: Program::GrokBot,
                project: project.map(Into::into),
                model: None,
                description: None,
            })
            .unwrap();
    }
    let ct = CancellationToken::new();
    let app = http_router(
        store.clone(),
        ct.clone(),
        &HttpOptions {
            auth: true,
            ..Default::default()
        },
    );
    let addr = serve_on_loopback(app, ct.clone()).await;
    Server {
        _dir: dir,
        store,
        base: format!("http://{addr}"),
        ct,
    }
}

/// A webhook receiver on loopback that records what it is sent and answers `status`.
async fn receiver(status: u16, ct: CancellationToken) -> (String, Received) {
    let got: Received = Arc::default();
    let sink = got.clone();
    let app = axum::Router::new().route(
        "/hook",
        axum::routing::post(move |headers: HeaderMap, body: String| {
            let sink = sink.clone();
            async move {
                sink.lock().unwrap().push((headers, body));
                axum::http::StatusCode::from_u16(status).unwrap()
            }
        }),
    );
    let addr = serve_on_loopback(app, ct).await;
    (format!("http://{addr}/hook"), got)
}

async fn connect(base: &str, token: &str) -> Client {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("{base}/mcp")).auth_header(token),
    );
    ClientConfig::default().serve(transport).await.unwrap()
}

async fn call(client: &Client, tool: &str, args: Value) -> CallToolResult {
    client
        .call_tool(
            CallToolRequestParams::new(tool.to_string())
                .with_arguments(args.as_object().cloned().unwrap()),
        )
        .await
        .unwrap()
}

fn text(r: &CallToolResult) -> Value {
    let t = serde_json::to_value(&r.content).unwrap()[0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    serde_json::from_str(&t).unwrap()
}

fn code(r: &CallToolResult) -> Option<String> {
    (r.is_error == Some(true))
        .then(|| text(r)["error"]["code"].as_str().map(str::to_string))
        .flatten()
}

fn lane(store: &Store, name: &str, project: &str, lead: &str) {
    store
        .create_lane(
            NewLane {
                agent: "manager".into(),
                name: name.into(),
                project: project.into(),
                lead: lead.into(),
                charter: String::new(),
                status: None,
            },
            None,
        )
        .unwrap();
}

async fn deliver(store: &Store) -> usize {
    let s = store.clone();
    tokio::task::spawn_blocking(move || another::push::deliver_once(&s))
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_webhooks_and_streams_respect_token_scope() {
    let srv = start().await;
    let (hook_url, received) = receiver(200, srv.ct.clone()).await;
    let shop = srv
        .store
        .create_token("manager", &["/work/shop".into()], None, None)
        .unwrap();
    let vault = srv
        .store
        .create_token("manager", &["/work/vault".into()], None, None)
        .unwrap();
    let client = connect(&srv.base, &shop.secret).await;

    // Subscribing: defaults to the token's globs; wider globs and all_projects are refused.
    let r = call(
        &client,
        "webhook_subscribe",
        json!({ "url": hook_url, "filters": ["lane_status", "needs_decision"] }),
    )
    .await;
    assert_eq!(code(&r), None, "{:?}", text(&r));
    let created = text(&r);
    let secret = created["secret"].as_str().unwrap().to_string();
    assert_eq!(created["webhook"]["project_globs"], json!(["/work/shop"]));
    let r = call(
        &client,
        "webhook_subscribe",
        json!({ "url": hook_url, "filters": ["all"], "projects": ["/work/vault"] }),
    )
    .await;
    assert_eq!(code(&r).as_deref(), Some("forbidden"));
    let r = call(
        &client,
        "webhook_subscribe",
        json!({ "url": hook_url, "filters": ["all"], "all_projects": true }),
    )
    .await;
    assert_eq!(code(&r).as_deref(), Some("forbidden"));
    let r = call(
        &client,
        "webhook_subscribe",
        json!({ "url": "http://hooks.example.test/x", "filters": ["all"] }),
    )
    .await;
    assert_eq!(code(&r).as_deref(), Some("invalid"));

    // Another token's webhook is invisible and cannot be removed.
    let vault_client = connect(&srv.base, &vault.secret).await;
    let r = call(
        &vault_client,
        "webhook_subscribe",
        json!({ "url": hook_url, "filters": ["lane_status"] }),
    )
    .await;
    let vault_hook = text(&r)["webhook"]["webhook_id"]
        .as_str()
        .unwrap()
        .to_string();
    let listed = text(&call(&client, "webhook_list", json!({})).await);
    assert_eq!(listed["webhooks"].as_array().unwrap().len(), 1);
    assert!(
        !listed.to_string().contains("whsec_"),
        "secrets never listed"
    );
    let r = call(
        &client,
        "webhook_remove",
        json!({ "webhook_id": vault_hook }),
    )
    .await;
    assert_eq!(code(&r).as_deref(), Some("forbidden"));

    // Events in both projects; each webhook receives only its own project's.
    lane(&srv.store, "shop-lane", "shop", "shopper");
    lane(&srv.store, "vault-lane", "vault", "vaulter");
    assert_eq!(deliver(&srv.store).await, 2);
    let got = received.lock().unwrap().clone();
    assert_eq!(got.len(), 2);
    let shop_hook = created["webhook"]["webhook_id"].as_str().unwrap();
    let (headers, body) = got
        .iter()
        .find(|(_, b)| b.contains(shop_hook))
        .expect("shop delivery")
        .clone();
    assert!(body.contains("shop-lane") && !body.contains("vault-lane"));
    let (_, vault_body) = got.iter().find(|(_, b)| b.contains(&vault_hook)).unwrap();
    assert!(vault_body.contains("vault-lane") && !vault_body.contains("shop-lane"));
    let h = |k: &str| headers.get(k).unwrap().to_str().unwrap().to_string();
    assert_eq!(h("x-another-event"), "lane.created");
    let ts: i64 = h("x-another-timestamp").parse().unwrap();
    assert_eq!(
        h("x-another-signature"),
        another_core::webhook_signature(&secret, ts, &body)
    );
    assert_eq!(deliver(&srv.store).await, 0, "delivered once");

    // MCP long-poll: only the shop lane's events.
    let r = call(
        &client,
        "events_poll",
        json!({ "since": "1h", "timeout_seconds": 1 }),
    )
    .await;
    let feed = text(&r).to_string();
    assert!(feed.contains("shop-lane"), "{feed}");
    assert!(!feed.contains("vault"), "{feed}");
    // From now on: times out empty, with a cursor.
    let r = call(&client, "events_poll", json!({ "timeout_seconds": 1 })).await;
    let empty = text(&r);
    assert_eq!(empty["items"], json!([]));
    assert!(empty["next_since"].is_string());

    // HTTP long-poll wakes up when an in-scope event arrives, not for out-of-scope ones.
    let http = reqwest::Client::new();
    assert_eq!(
        http.get(format!("{}/events/poll", srv.base))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let cursor = empty["next_since"].as_str().unwrap().to_string();
    let poll = {
        let http = http.clone();
        let url = format!("{}/events/poll?since={cursor}&timeout=10", srv.base);
        let token = shop.secret.clone();
        tokio::spawn(async move {
            http.get(url)
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    lane(&srv.store, "vault-two", "vault", "vaulter");
    lane(&srv.store, "shop-two", "shop", "shopper");
    let woke = poll.await.unwrap();
    let items = woke["items"].as_array().unwrap();
    assert!(!items.is_empty());
    assert!(woke.to_string().contains("shop-two"));
    assert!(!woke.to_string().contains("vault-two"));

    // SSE: replay since a duration; only in-scope events, with ids.
    let mut resp = http
        .get(format!("{}/events/stream?since=1h&kinds=lane", srv.base))
        .bearer_auth(&shop.secret)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    let mut sse = String::new();
    while !sse.contains("shop-two") {
        let chunk = tokio::time::timeout(Duration::from_secs(10), resp.chunk())
            .await
            .expect("stream produced events")
            .unwrap()
            .expect("stream open");
        sse.push_str(&String::from_utf8_lossy(&chunk));
    }
    drop(resp);
    assert!(sse.contains("event: lane.created"));
    assert!(sse.contains("id: "));
    assert!(sse.contains("shop-lane"));
    assert!(!sse.contains("vault"), "{sse}");

    // Every remote push read and webhook call is audited.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let audit = srv.store.audit_log(200).unwrap();
    let ops: Vec<(String, String)> = audit
        .iter()
        .map(|a| (a.operation.clone(), a.outcome.clone()))
        .collect();
    for (op, outcome) in [
        ("GET /events/poll", "ok"),
        ("GET /events/poll", "denied"),
        ("GET /events/stream", "ok"),
        ("tools/call webhook_subscribe", "ok"),
        ("tools/call webhook_subscribe", "error"),
        ("tools/call events_poll", "ok"),
        ("tools/call webhook_remove", "error"),
    ] {
        assert!(
            ops.contains(&(op.to_string(), outcome.to_string())),
            "missing audit {op} {outcome}: {ops:?}"
        );
    }
    client.cancel().await.unwrap();
    vault_client.cancel().await.unwrap();
    srv.ct.cancel();
}

fn bin(home: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_another"));
    c.args(args)
        .env("ANOTHER_HOME", home)
        .env("HOME", home)
        .env_remove("ANOTHER_AGENT")
        .stdin(Stdio::null());
    c
}

#[tokio::test(flavor = "multi_thread")]
async fn webhook_cli() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    let ct = CancellationToken::new();
    let (ok_url, received) = receiver(204, ct.clone()).await;
    let (bad_url, _) = receiver(500, ct.clone()).await;

    let run = |args: Vec<String>| {
        let home = home.clone();
        async move {
            tokio::task::spawn_blocking(move || {
                let a: Vec<&str> = args.iter().map(String::as_str).collect();
                bin(&home, &a).output().unwrap()
            })
            .await
            .unwrap()
        }
    };
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();

    // Projects are required (or an explicit --all-projects).
    let out = run(s(&["webhook", "add", "--url", &ok_url, "--events", "all"])).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--all-projects"));
    let out = run(s(&[
        "--json",
        "webhook",
        "add",
        "--url",
        &ok_url,
        "--events",
        "lane_status,hold_conflict",
        "--project",
        "/work/*",
    ]))
    .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let created: Value = serde_json::from_slice(&out.stdout).unwrap();
    let id = created["webhook"]["webhook_id"]
        .as_str()
        .unwrap()
        .to_string();
    let secret = created["secret"].as_str().unwrap().to_string();
    let out = run(s(&[
        "webhook",
        "add",
        "--url",
        "http://example.test/x",
        "--events",
        "all",
        "--all-projects",
    ]))
    .await;
    assert!(
        !out.status.success(),
        "plain http to a non-loopback host is refused"
    );

    let out = run(s(&["webhook", "list"])).await;
    let listed = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(listed.contains(&id) && listed.contains("lane_status,hold_conflict"));
    assert!(!listed.contains(&secret));

    // test: a signed delivery, synchronously.
    let out = run(s(&["webhook", "test", &id])).await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    {
        let got = received.lock().unwrap();
        let (h, body) = got.last().unwrap();
        assert_eq!(h["x-another-event"], "webhook.test");
        let ts: i64 = h["x-another-timestamp"].to_str().unwrap().parse().unwrap();
        assert_eq!(
            h["x-another-signature"].to_str().unwrap(),
            another_core::webhook_signature(&secret, ts, body)
        );
    }

    // A failing endpoint: test exits non-zero and records the error.
    let out = run(s(&[
        "--json",
        "webhook",
        "add",
        "--url",
        &bad_url,
        "--events",
        "all",
        "--all-projects",
    ]))
    .await;
    let bad: Value = serde_json::from_slice(&out.stdout).unwrap();
    let bad_id = bad["webhook"]["webhook_id"].as_str().unwrap().to_string();
    let out = run(s(&["webhook", "test", &bad_id])).await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("HTTP 500"));
    let out = run(s(&["webhook", "dead"])).await;
    assert!(String::from_utf8_lossy(&out.stdout).contains("no deliveries"));

    let out = run(s(&["webhook", "remove", &bad_id])).await;
    assert!(out.status.success());
    let out = run(s(&["webhook", "deliveries", "--webhook", &bad_id])).await;
    assert!(String::from_utf8_lossy(&out.stdout).contains("dead"));
    ct.cancel();
}

/// Project-less events are matched by agent id, not name: another node's `manager` is not
/// this token's `manager`.
#[test]
fn same_name_on_another_node_is_not_the_token_agent() {
    use another::fed;
    use another_core::{ActivityQuery, Perm, SessionInfo, SessionPhase, Since};
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let hub_dir = tempfile::tempdir().unwrap();
    let hub = Store::open(hub_dir.path()).unwrap();
    hub.set_node_name("hub").unwrap();
    hub.ensure_project("t", "/work/shop").unwrap();
    hub.register_agent(RegisterAgent {
        name: "manager".into(),
        program: Program::GrokBot,
        project: None,
        model: None,
        description: None,
    })
    .unwrap();
    let ct = CancellationToken::new();
    let app = http_router(
        hub.clone(),
        ct.clone(),
        &HttpOptions {
            federation: true,
            ..Default::default()
        },
    );
    let addr = rt.block_on(async {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let stop = ct.clone();
        tokio::spawn(async move {
            axum::serve(l, app.into_make_service_with_connect_info::<SocketAddr>())
                .with_graceful_shutdown(async move { stop.cancelled_owned().await })
                .await
                .unwrap();
        });
        addr
    });
    let url = format!("http://{addr}");
    let spoke_dir = tempfile::tempdir().unwrap();
    let spoke = Store::open(spoke_dir.path()).unwrap();
    spoke.set_node_name("laptop").unwrap();
    spoke
        .register_agent(RegisterAgent {
            name: "manager".into(),
            program: Program::GrokBot,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    let code = hub.create_invite(&url, &[Perm::ReadFeed], 3600).unwrap();
    fed::join(&spoke, &code).unwrap();
    spoke
        .session("manager", SessionPhase::Start, SessionInfo::default())
        .unwrap();
    let r = fed::sync(&spoke).unwrap();
    assert!(r.push_rejected.is_empty(), "{:?}", r.push_rejected);

    // The hub stored the laptop's `manager` session event (actor "manager")…
    let all = hub
        .activity(ActivityQuery {
            limit: 500,
            ..Default::default()
        })
        .unwrap();
    let spoke_node = spoke.node_id().to_string();
    assert!(all
        .items
        .iter()
        .any(|i| i.kind == "session.started" && i.node_id == spoke_node));
    // …but the hub manager's token does not see it as its own.
    let t = hub
        .create_token("manager", &["/work/shop".into()], None, None)
        .unwrap();
    let scope = hub.verify_token(&t.secret).unwrap();
    let mine = hub
        .activity_scoped(
            ActivityQuery {
                since: Some(Since::Time(0)),
                limit: 500,
                kinds: vec![],
            },
            Some(&scope),
        )
        .unwrap();
    assert!(
        mine.items.iter().all(|i| i.node_id != spoke_node),
        "{:?}",
        mine.items
    );
    // A local session of the hub's own manager still is.
    hub.session("manager", SessionPhase::Start, SessionInfo::default())
        .unwrap();
    let mine = hub
        .activity_scoped(
            ActivityQuery {
                since: Some(Since::Time(0)),
                limit: 500,
                kinds: vec![],
            },
            Some(&scope),
        )
        .unwrap();
    assert!(mine.items.iter().any(|i| i.kind == "session.started"));
    ct.cancel();
}
