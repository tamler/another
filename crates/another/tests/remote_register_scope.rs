//! A remote token may register only its own agent, in its own project: a token for
//! `alice@shop` registers `alice` in `shop` and nowhere else, and a token
//! for a project-less agent may not attach a project. `threads` without a thread id acts as
//! the token's agent only, and a tool name the server does not know is refused.

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
    _ct: CancellationToken,
    work: String,
    work_glob: String,
}

async fn start() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let work = dir.path().join("work/shop").to_string_lossy().into_owned();
    let work_glob = dir
        .path()
        .join("work/*")
        .to_string_lossy()
        .replace('\\', "/");
    store.ensure_project("t", &work).unwrap();
    for (name, program, project) in [
        ("grok-bot", Program::GrokBot, None),
        ("alice", Program::Claude, Some(work.as_str())),
    ] {
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
    let ct = CancellationToken::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let opts = HttpOptions {
        auth: true,
        bound_port: addr.port(),
        ..Default::default()
    };
    let app = http_router(store.clone(), ct.clone(), &opts);
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
        work,
        work_glob,
        _ct: ct,
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

fn error_code(res: &CallToolResult) -> Option<String> {
    if res.is_error != Some(true) {
        return None;
    }
    let text = serde_json::to_value(&res.content).unwrap()[0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    let v: Value = serde_json::from_str(&text).unwrap();
    v["error"]["code"].as_str().map(str::to_string)
}

fn message(res: &CallToolResult) -> String {
    serde_json::to_value(&res.content).unwrap()[0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test]
async fn a_project_scoped_token_registers_its_agent_only_in_its_project() {
    let srv = start().await;
    // A second project inside the token's globs, so the slug check (not the globs) decides.
    let other = std::path::Path::new(&srv.work)
        .with_file_name("other")
        .to_string_lossy()
        .into_owned();
    srv.store.ensure_project("t", &other).unwrap();
    let token = srv
        .store
        .create_token(
            "alice@shop",
            std::slice::from_ref(&srv.work_glob),
            None,
            None,
        )
        .unwrap();
    let client = connect(&srv.base, &token.secret).await;

    let ok = call(
        &client,
        "register_agent",
        json!({ "name": "alice", "program": "claude", "project": srv.work }),
    )
    .await;
    assert_eq!(error_code(&ok), None, "{}", message(&ok));
    for args in [
        json!({ "name": "alice", "program": "claude", "project": other }),
        json!({ "name": "alice", "program": "claude" }),
    ] {
        let res = call(&client, "register_agent", args.clone()).await;
        assert_eq!(error_code(&res).as_deref(), Some("forbidden"), "{args}");
        assert!(
            message(&res).contains("can only register alice@shop"),
            "{args}"
        );
    }
    // Nothing named alice was registered in the other project or without one.
    let alices: Vec<String> = srv
        .store
        .agents()
        .unwrap()
        .into_iter()
        .filter(|a| a.name == "alice")
        .map(|a| a.address)
        .collect();
    assert_eq!(alices, ["alice@shop"]);
}

#[tokio::test]
async fn a_projectless_token_may_not_attach_a_project() {
    let srv = start().await;
    let token = srv
        .store
        .create_token("grok-bot", std::slice::from_ref(&srv.work_glob), None, None)
        .unwrap();
    let client = connect(&srv.base, &token.secret).await;
    let res = call(
        &client,
        "register_agent",
        json!({ "name": "grok-bot", "program": "grok_bot", "project": srv.work }),
    )
    .await;
    assert_eq!(
        error_code(&res).as_deref(),
        Some("forbidden"),
        "{}",
        message(&res)
    );
    assert!(srv.store.agent("grok-bot@shop").is_err());
}

#[tokio::test]
async fn threads_without_an_id_and_unknown_tools() {
    let srv = start().await;
    let token = srv.store.create_token("grok-bot", &[], None, None).unwrap();
    let client = connect(&srv.base, &token.secret).await;
    let mine = call(&client, "threads", json!({ "agent": "grok-bot" })).await;
    assert_eq!(error_code(&mine), None, "{}", message(&mine));
    let theirs = call(&client, "threads", json!({ "agent": "alice@shop" })).await;
    assert_eq!(
        error_code(&theirs).as_deref(),
        Some("forbidden"),
        "{}",
        message(&theirs)
    );

    let unknown = call(&client, "no_such_tool", json!({})).await;
    assert_eq!(error_code(&unknown).as_deref(), Some("forbidden"));
    assert!(
        message(&unknown).contains("is not available on this connection"),
        "{}",
        message(&unknown)
    );
}

/// The token's own project must also be inside its globs: a token for `alice@shop` whose
/// globs leave out `shop` cannot register `alice` there, though the slug matches.
#[tokio::test]
async fn a_token_cannot_register_into_its_project_outside_its_globs() {
    let srv = start().await;
    let elsewhere = std::path::Path::new(&srv.work)
        .parent()
        .unwrap()
        .with_file_name("elsewhere")
        .join("*")
        .to_string_lossy()
        .replace('\\', "/");
    let token = srv
        .store
        .create_token("alice@shop", &[elsewhere], None, None)
        .unwrap();
    let client = connect(&srv.base, &token.secret).await;
    let res = call(
        &client,
        "register_agent",
        json!({ "name": "alice", "program": "claude", "project": srv.work }),
    )
    .await;
    assert_eq!(
        error_code(&res).as_deref(),
        Some("forbidden"),
        "{}",
        message(&res)
    );
    assert!(
        message(&res).contains("outside this connection's project globs"),
        "{}",
        message(&res)
    );
}
