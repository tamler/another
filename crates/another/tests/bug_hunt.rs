//! Regression tests from the v0.1.0 bug hunt (`another-bug-hunt-BUGS.md`), one per finding,
//! named after its bug number. An authenticated server in process, on an ephemeral loopback
//! port, with a temporary home.

use std::net::SocketAddr;

use another::serve::{http_router, HttpOptions};
use another_core::{Program, RegisterAgent, Store};
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

type Client = RunningService<RoleClient, ClientConfig>;

struct Server {
    _dir: tempfile::TempDir,
    store: Store,
    base: String,
    ct: CancellationToken,
}

fn agent(store: &Store, name: &str, program: Program, project: Option<&str>) {
    store
        .register_agent(RegisterAgent {
            name: name.into(),
            program,
            project: project.map(Into::into),
            model: None,
            description: None,
        })
        .unwrap();
}

async fn start() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.ensure_project("t", "/sandbox/w/a").unwrap();
    store.ensure_project("t", "/private/data").unwrap();
    agent(&store, "worker", Program::Claude, Some("/sandbox/w/a"));
    agent(&store, "insider", Program::Claude, Some("/private/data"));
    agent(&store, "colleague", Program::Claude, Some("/private/data"));
    let ct = CancellationToken::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = http_router(
        store.clone(),
        ct.clone(),
        &HttpOptions {
            auth: true,
            bound_port: addr.port(),
            ..Default::default()
        },
    );
    let stop = ct.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { stop.cancelled_owned().await })
        .await
        .unwrap();
    });
    Server {
        _dir: dir,
        store,
        base: format!("http://{addr}"),
        ct,
    }
}

impl Server {
    /// A token for `worker@a` that the project globs keep inside `/sandbox/w`.
    fn token(&self) -> another_core::NewToken {
        self.store
            .create_token("worker@a", &["/sandbox/w/**".to_string()], None, None)
            .unwrap()
    }
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

fn text(result: &CallToolResult) -> String {
    serde_json::to_string(&result.content).unwrap()
}

/// BH-1. The token's globs are checked against the raw string and the store then normalizes it,
/// so `/sandbox/w/../../private/data` passes the check and lands in `/private/data`.
#[tokio::test]
async fn bh1_dot_dot_does_not_escape_a_tokens_project_globs() {
    let srv = start().await;
    let client = connect(&srv.base, &srv.token().secret).await;
    let escape = "/sandbox/w/../../private/data";

    let refused = call(
        &client,
        "hold_paths",
        json!({ "project": "/private/data", "paths": ["x"] }),
    )
    .await;
    assert_eq!(refused.is_error, Some(true), "the plain path is refused");

    let hold = call(
        &client,
        "hold_paths",
        json!({ "project": escape, "paths": ["x"] }),
    )
    .await;
    assert_eq!(
        hold.is_error,
        Some(true),
        "hold outside the globs: {hold:?}"
    );
    assert!(srv.store.holds(Default::default()).unwrap().is_empty());

    let resource = call(
        &client,
        "resource_register",
        json!({ "project": escape, "kind": "model_server", "name": "r", "ttl_seconds": 60 }),
    )
    .await;
    assert_eq!(
        resource.is_error,
        Some(true),
        "resource outside the globs: {resource:?}"
    );

    for path in [escape, "/sandbox/w/../../brand/new"] {
        let ensured = call(&client, "ensure_project", json!({ "path": path })).await;
        assert_eq!(
            ensured.is_error,
            Some(true),
            "{path} reads or creates a project outside the globs: {ensured:?}"
        );
    }
    assert!(
        !srv.store
            .projects()
            .unwrap()
            .iter()
            .any(|p| p.abs_path == "/brand/new"),
        "project created outside the globs"
    );

    // A path that normalizes to somewhere inside the globs still works.
    let inside = call(
        &client,
        "hold_paths",
        json!({ "project": "/sandbox/w/x/../a", "paths": ["y"] }),
    )
    .await;
    assert_ne!(inside.is_error, Some(true), "{inside:?}");
    srv.ct.cancel();
}

/// BH-3. `claim_request` only checks who is acting: a token can claim or complete any request
/// on the node, however far outside its globs, and is told its title.
#[tokio::test]
async fn bh3_a_token_cannot_touch_requests_outside_its_scope() {
    let srv = start().await;
    let client = connect(&srv.base, &srv.token().secret).await;
    let ask = |from: &str, to: &str, subject: &str| {
        srv.store
            .send_message(another_core::SendMessage {
                from: from.into(),
                to: vec![to.into()],
                subject: subject.into(),
                body: "private".into(),
                request: Some(Default::default()),
                ..Default::default()
            })
            .unwrap()
            .request_id
            .unwrap()
    };
    let claim = ask("insider@data", "colleague@data", "private claim");
    let complete = ask("insider@data", "colleague@data", "private complete");

    let claimed = call(&client, "claim_request", json!({ "request_id": claim })).await;
    assert_eq!(claimed.is_error, Some(true), "claimed: {claimed:?}");
    let forged = call(
        &client,
        "claim_request",
        json!({ "request_id": complete, "action": "complete", "note": "forged" }),
    )
    .await;
    assert_eq!(forged.is_error, Some(true), "completed: {forged:?}");

    assert_eq!(srv.store.request(&claim).unwrap().owner, None);
    assert_eq!(
        srv.store.request(&complete).unwrap().status.as_str(),
        "open"
    );

    // Out of scope looks exactly like missing, and reveals nothing about the request.
    let missing = "01JZZZZZZZZZZZZZZZZZZZZZZZ";
    let absent = call(&client, "claim_request", json!({ "request_id": missing })).await;
    assert_eq!(
        text(&claimed).replace(&claim, "ID"),
        text(&absent).replace(missing, "ID"),
        "an out-of-scope request does not look missing"
    );
    assert!(text(&claimed).contains("not_found"), "{claimed:?}");
    assert!(!text(&claimed).contains("private claim"), "{claimed:?}");

    // A request addressed to the token's agent can still be claimed and completed.
    let mine = ask("insider@data", "worker@a", "for worker");
    let ok = call(
        &client,
        "claim_request",
        json!({ "request_id": mine, "action": "complete" }),
    )
    .await;
    assert_ne!(ok.is_error, Some(true), "{ok:?}");
    assert_eq!(srv.store.request(&mine).unwrap().status.as_str(), "done");
    srv.ct.cancel();
}

/// BH-12. `threads` is limited to threads the token's agent is in, but `send_message` lets it
/// join any thread by replying (or naming the thread), and that then opens the whole history.
#[tokio::test]
async fn bh12_a_reply_does_not_open_a_thread_the_token_cannot_read() {
    let srv = start().await;
    let client = connect(&srv.base, &srv.token().secret).await;
    let private = srv
        .store
        .send_message(another_core::SendMessage {
            from: "insider@data".into(),
            to: vec!["colleague@data".into()],
            subject: "private subject".into(),
            body: "the private body".into(),
            ..Default::default()
        })
        .unwrap();
    let read = json!({ "thread_id": private.thread_id });

    let before = call(&client, "threads", read.clone()).await;
    assert_eq!(before.is_error, Some(true), "not a participant yet");

    let attempts = [
        json!({
            "to": ["insider@data"],
            "subject": "hello",
            "body": "let me in",
            "reply_to": private.message_id,
        }),
        // No subject: the tool would otherwise copy the private one.
        json!({ "to": ["insider@data"], "body": "let me in", "reply_to": private.message_id }),
        // No recipients: a plain reply to the private sender.
        json!({ "body": "let me in", "reply_to": private.message_id }),
    ];
    for args in attempts {
        let sent = call(&client, "send_message", args.clone()).await;
        assert_eq!(sent.is_error, Some(true), "{args}: {sent:?}");
        let t = text(&sent);
        assert!(t.contains("not_found"), "{args}: {t}");
        assert!(!t.contains("private subject"), "{args}: {t}");
    }
    // `send_message` has no `thread_id` argument; naming one must not post into the thread.
    let _ = call(
        &client,
        "send_message",
        json!({
            "to": ["insider@data"],
            "subject": "hello",
            "body": "let me in",
            "thread_id": private.thread_id,
        }),
    )
    .await;
    let after = call(&client, "threads", read).await;
    let t = text(&after);
    assert!(
        !t.contains("the private body"),
        "a reply opened the private thread: {t}"
    );
    assert_eq!(
        srv.store.thread(&private.thread_id).unwrap().messages.len(),
        1
    );

    // A thread the token's agent is in can still be replied to.
    let mine = srv
        .store
        .send_message(another_core::SendMessage {
            from: "insider@data".into(),
            to: vec!["worker@a".into()],
            subject: "for worker".into(),
            body: "hi".into(),
            ..Default::default()
        })
        .unwrap();
    let reply = call(
        &client,
        "send_message",
        json!({ "body": "hi back", "reply_to": mine.message_id }),
    )
    .await;
    assert_ne!(reply.is_error, Some(true), "{reply:?}");
    srv.ct.cancel();
}

/// BH-13. `authorize` checks the `project` argument, but renew and release ignore it and act on
/// every hold the agent has: an in-scope project unlocks holds outside the globs.
#[tokio::test]
async fn bh13_renew_and_release_stay_inside_the_tokens_globs() {
    let srv = start().await;
    let client = connect(&srv.base, &srv.token().secret).await;
    let hold = |project: &str, path: &str| acquire(&srv.store, "worker@a", project, path);
    let outside = hold("/private/data", "local-work.rs");
    let outside_expiry = live_holds(&srv.store)[0].1.clone();
    let untouched = || vec![(outside.clone(), outside_expiry.clone())];
    let inside = hold("/sandbox/w/a", "remote-work.rs");

    // Renew all: only the in-scope hold, and the other one's expiry does not move.
    let renew = call(
        &client,
        "hold_paths",
        json!({ "action": "renew", "project": "/sandbox/w/a" }),
    )
    .await;
    assert_ne!(renew.is_error, Some(true), "{renew:?}");
    assert_eq!(
        ids(&out(&renew)["renewed"]),
        vec![inside.clone()],
        "{renew:?}"
    );
    let live = live_holds(&srv.store);
    assert!(live.contains(&untouched()[0]), "renew moved it: {live:?}");

    // Naming the hidden hold looks exactly like naming an unknown one.
    let unknown = "01JZZZZZZZZZZZZZZZZZZZZZZZ";
    for action in ["renew", "release"] {
        let by_id =
            |id: &str| json!({ "action": action, "project": "/sandbox/w/a", "hold_ids": [id] });
        let named = call(&client, "hold_paths", by_id(&outside)).await;
        let absent = call(&client, "hold_paths", by_id(unknown)).await;
        assert_eq!(named.is_error, Some(true), "{action} by id: {named:?}");
        assert!(text(&named).contains("not_found"), "{named:?}");
        assert_eq!(
            text(&named).replace(&outside, "ID"),
            text(&absent).replace(unknown, "ID"),
            "{action}: a hidden hold does not look missing"
        );
    }

    // Release all: only the in-scope hold.
    let release = call(
        &client,
        "hold_paths",
        json!({ "action": "release", "project": "/sandbox/w/a" }),
    )
    .await;
    assert_ne!(release.is_error, Some(true), "{release:?}");
    assert_eq!(out(&release)["released"], json!([inside]), "{release:?}");
    assert_eq!(live_holds(&srv.store), untouched(), "release");

    // Heartbeat, on its own: renews and then ends only the in-scope hold.
    let again = hold("/sandbox/w/a", "remote-work.rs");
    let beat = call(&client, "heartbeat", json!({})).await;
    assert_ne!(beat.is_error, Some(true), "{beat:?}");
    assert_eq!(
        ids(&out(&beat)["renewed_holds"]),
        vec![again.clone()],
        "heartbeat: {beat:?}"
    );
    let end = call(&client, "heartbeat", json!({ "end": true })).await;
    assert_ne!(end.is_error, Some(true), "{end:?}");
    assert_eq!(out(&end)["released_holds"], json!([again]), "{end:?}");
    assert_eq!(live_holds(&srv.store), untouched(), "heartbeat end");
    srv.ct.cancel();
}

/// BH-13, for resource leases. A remote heartbeat renews, and its end releases, only the
/// leases on resources in the token's projects; the agent's other leases are left alone.
#[tokio::test]
async fn bh13_heartbeat_leases_stay_inside_the_tokens_globs() {
    let srv = start().await;
    let client = connect(&srv.base, &srv.token().secret).await;
    let register = |project: &str, name: &str| {
        srv.store
            .register_resource(another_core::RegisterResource {
                agent: "worker@a".into(),
                kind: another_core::ResourceKind::LongJob,
                name: name.into(),
                endpoint: None,
                port: None,
                pid: None,
                project: Some(project.into()),
                purpose: None,
                mode: Default::default(),
                ttl_seconds: Some(600),
                metadata: None,
            })
            .unwrap()
            .resource
            .resource_id
    };
    let hidden = register("/private/data", "local-job");
    let visible = register("/sandbox/w/a", "remote-job");
    let leases = |id: &str| {
        srv.store
            .resource(&another_core::ResourceRef::Id(id.into()))
            .unwrap()
            .leases
            .into_iter()
            .map(|l| (l.lease_id, l.expires_at))
            .collect::<Vec<_>>()
    };
    let hidden_leases = leases(&hidden);
    assert_eq!(hidden_leases.len(), 1);

    let beat = call(&client, "heartbeat", json!({})).await;
    assert_ne!(beat.is_error, Some(true), "{beat:?}");
    assert_eq!(out(&beat)["resource_leases"], json!(1), "beat: {beat:?}");
    assert_eq!(
        leases(&hidden),
        hidden_leases,
        "beat renewed a hidden lease"
    );

    let end = call(&client, "heartbeat", json!({ "end": true })).await;
    assert_ne!(end.is_error, Some(true), "{end:?}");
    assert_eq!(out(&end)["resource_leases"], json!(1), "end: {end:?}");
    assert!(leases(&visible).is_empty(), "the visible lease is released");
    assert_eq!(
        leases(&hidden),
        hidden_leases,
        "end released a hidden lease"
    );
    srv.ct.cancel();
}

fn out(result: &CallToolResult) -> Value {
    result
        .structured_content
        .clone()
        .expect("structured output")
}

/// The `hold_id`s of a list of holds in a tool result.
fn ids(holds: &Value) -> Vec<String> {
    holds
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["hold_id"].as_str().unwrap().to_string())
        .collect()
}

fn acquire(store: &Store, agent: &str, project: &str, path: &str) -> String {
    store
        .acquire_holds(another_core::AcquireHolds {
            agent: agent.into(),
            project: project.into(),
            paths: vec![path.into()],
            exclusive: true,
            ttl_seconds: Some(600),
            reason: None,
        })
        .unwrap()
        .granted[0]
        .hold_id
        .clone()
}

/// Live holds as (id, expiry), read from the store.
fn live_holds(store: &Store) -> Vec<(String, String)> {
    store
        .holds(Default::default())
        .unwrap()
        .into_iter()
        .map(|h| (h.hold_id, h.expires_at))
        .collect()
}
