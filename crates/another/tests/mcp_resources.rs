//! The resource tools (`resource_register`, `resource_find`, `resource_borrow`,
//! `resource_release`, `resource_heartbeat`) through a real rmcp client: the lease lifecycle,
//! their error codes, and what a remote bearer token may do with them.

use std::net::SocketAddr;

use another::mcp::AnotherMcp;
use another::serve::{http_router, HttpOptions};
use another_core::{Clock, Program, RegisterAgent, Store};
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

type Client = RunningService<RoleClient, ClientConfig>;

async fn call(client: &Client, tool: &str, args: Value) -> CallToolResult {
    client
        .call_tool(
            CallToolRequestParams::new(tool.to_string())
                .with_arguments(args.as_object().cloned().expect("object args")),
        )
        .await
        .expect("tool call")
}

async fn ok(client: &Client, tool: &str, args: Value) -> Value {
    let res = call(client, tool, args).await;
    assert_ne!(res.is_error, Some(true), "{tool} failed: {res:?}");
    res.structured_content.expect("structured output")
}

/// The error code of a failed call, `None` when it succeeded.
fn code(res: &CallToolResult) -> Option<String> {
    if res.is_error != Some(true) {
        return None;
    }
    let text = serde_json::to_value(&res.content).unwrap()[0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    // rmcp reports a parameter error as plain text, not JSON: show it rather than the
    // parse error (FINDINGS-157).
    let body: Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("tool error is not JSON ({e}): {text}"));
    body["error"]["code"].as_str().map(str::to_string)
}

async fn err(client: &Client, tool: &str, args: Value) -> String {
    let res = call(client, tool, args).await;
    code(&res).unwrap_or_else(|| panic!("{tool} should fail: {res:?}"))
}

async fn in_process(store: Store) -> Client {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let server = AnotherMcp::new(store).serve(server_io).await.unwrap();
        let _ = server.waiting().await;
    });
    ClientConfig::default().serve(client_io).await.unwrap()
}

fn register(store: &Store, name: &str, program: Program, project: Option<&str>) {
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

fn agents(resource: &Value) -> Vec<(String, String)> {
    resource["leases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            (
                l["agent"].as_str().unwrap().to_string(),
                l["role"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_shared_resource_lives_through_borrow_renew_transfer_and_stop() {
    let store = Store::open_in_memory(Clock::System).unwrap();
    for name in ["alice", "bob", "carol"] {
        register(&store, name, Program::Claude, None);
    }
    let client = in_process(store).await;

    let reg = ok(
        &client,
        "resource_register",
        json!({
            "agent": "alice", "kind": "model_server", "name": "ollama@11434",
            "endpoint": "http://127.0.0.1:11434", "purpose": "local llm",
            "metadata": { "model": "small" },
        }),
    )
    .await;
    assert_eq!(reg["outcome"], "registered");
    assert_eq!(reg["lease"]["role"], "owner");
    assert_eq!(reg["resource"]["port"], 11434, "port comes from the name");
    assert_eq!(reg["resource"]["mode"], "shared");
    let id = reg["resource"]["resource_id"].as_str().unwrap().to_string();

    // Found by a substring of the name, endpoint or purpose, and filtered by kind.
    for query in ["ollama", "11434", "LOCAL LLM"] {
        let found = ok(&client, "resource_find", json!({ "query": query })).await;
        assert_eq!(found["resources"].as_array().unwrap().len(), 1, "{query}");
    }
    let none = ok(&client, "resource_find", json!({ "kind": "gpu" })).await;
    assert!(none["resources"].as_array().unwrap().is_empty());

    // Borrowed by `kind:name` and by id; a second borrow renews.
    let b = ok(
        &client,
        "resource_borrow",
        json!({ "agent": "bob", "resource": "model_server:ollama@11434" }),
    )
    .await;
    assert_eq!(b["outcome"], "borrowed");
    assert_eq!(b["resource"]["refcount"], 2);
    let again = ok(
        &client,
        "resource_borrow",
        json!({ "agent": "bob", "resource": id, "ttl_seconds": 120 }),
    )
    .await;
    assert_eq!(again["outcome"], "renewed");
    assert_eq!(again["resource"]["refcount"], 2);
    ok(
        &client,
        "resource_borrow",
        json!({ "agent": "carol", "resource": id }),
    )
    .await;

    // Heartbeat renews the caller's leases: all of them, or only the listed ones.
    let hb = ok(&client, "resource_heartbeat", json!({ "agent": "bob" })).await;
    assert_eq!(hb["resources"].as_array().unwrap().len(), 1);
    let hb = ok(
        &client,
        "resource_heartbeat",
        json!({ "agent": "bob", "resources": ["model_server:ollama@11434"] }),
    )
    .await;
    assert_eq!(hb["resources"][0]["resource_id"], id);

    // Only the owner may stop it; a borrower is refused.
    assert_eq!(
        err(
            &client,
            "resource_release",
            json!({ "agent": "bob", "resource": id, "stopped": true }),
        )
        .await,
        "forbidden"
    );
    // The owner leaving passes ownership to the oldest borrower.
    let left = ok(
        &client,
        "resource_release",
        json!({ "agent": "alice", "resource": id }),
    )
    .await;
    assert!(left["lease"].is_null(), "{left}");
    assert_eq!(left["resource"]["owner"], "bob");
    assert_eq!(left["resource"]["refcount"], 2);
    assert!(agents(&left["resource"]).contains(&("bob".into(), "owner".into())));
    // Releasing a lease you do not hold is not found.
    assert_eq!(
        err(
            &client,
            "resource_release",
            json!({ "agent": "alice", "resource": id }),
        )
        .await,
        "not_found"
    );

    // The new owner stops it: every lease ends and it leaves the default listing.
    let stopped = ok(
        &client,
        "resource_release",
        json!({ "agent": "bob", "resource": id, "stopped": true }),
    )
    .await;
    assert_eq!(stopped["outcome"], "stopped");
    assert_eq!(stopped["resource"]["status"], "released");
    assert_eq!(stopped["resource"]["refcount"], 0);
    let found = ok(&client, "resource_find", json!({ "query": "ollama" })).await;
    assert!(found["resources"].as_array().unwrap().is_empty());
    let all = ok(
        &client,
        "resource_find",
        json!({ "query": "ollama", "include_released": true }),
    )
    .await;
    assert_eq!(all["resources"][0]["status"], "released");
}

#[tokio::test]
async fn resource_tools_report_errors_with_codes() {
    let store = Store::open_in_memory(Clock::System).unwrap();
    for name in ["alice", "bob"] {
        register(&store, name, Program::Codex, None);
    }
    let client = in_process(store).await;

    // An exclusive resource refuses borrowers.
    ok(
        &client,
        "resource_register",
        json!({ "agent": "alice", "kind": "gpu", "name": "metal:0", "exclusive": true }),
    )
    .await;
    assert_eq!(
        err(
            &client,
            "resource_borrow",
            json!({ "agent": "bob", "resource": "gpu:metal:0" }),
        )
        .await,
        "conflict"
    );
    // Unknown resources, references and agents.
    assert_eq!(
        err(
            &client,
            "resource_borrow",
            json!({ "agent": "bob", "resource": "dev_server:vite@5173" }),
        )
        .await,
        "not_found"
    );
    assert_eq!(
        err(
            &client,
            "resource_heartbeat",
            json!({ "agent": "bob", "resources": ["no-such-id"] }),
        )
        .await,
        "not_found"
    );
    assert_eq!(
        err(
            &client,
            "resource_borrow",
            json!({ "agent": "nobody", "resource": "gpu:metal:0" }),
        )
        .await,
        "not_found"
    );
    // Bad input: a non-positive TTL, and metadata that is not an object.
    for args in [
        json!({ "agent": "alice", "kind": "port", "name": "tcp:127.0.0.1:5432", "ttl_seconds": 0 }),
        json!({ "agent": "alice", "kind": "port", "name": "tcp:127.0.0.1:5433", "metadata": [1, 2] }),
    ] {
        assert_eq!(
            err(&client, "resource_register", args.clone()).await,
            "invalid",
            "{args}"
        );
    }
    // An unknown kind is rejected by the input schema.
    let res = client
        .call_tool(
            CallToolRequestParams::new("resource_register").with_arguments(
                json!({ "agent": "alice", "kind": "toaster", "name": "x" })
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .await;
    assert!(res.is_err() || res.unwrap().is_error == Some(true));
    // Nothing above registered a resource it refused.
    let all = ok(
        &client,
        "resource_find",
        json!({ "include_released": true }),
    )
    .await;
    assert_eq!(all["resources"].as_array().unwrap().len(), 1);
}

struct Remote {
    _dir: tempfile::TempDir,
    store: Store,
    base: String,
    ct: CancellationToken,
    work: String,
    secret: String,
    glob: String,
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.ct.cancel();
    }
}

async fn remote() -> Remote {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let path = |p: &str| dir.path().join(p).to_string_lossy().into_owned();
    let (work, secret) = (path("work/shop"), path("secret/vault"));
    let glob = path("work/*").replace('\\', "/");
    store.ensure_project("t", &work).unwrap();
    store.ensure_project("t", &secret).unwrap();
    register(&store, "grok-bot", Program::GrokBot, None);
    register(&store, "alice", Program::Claude, Some(&work));
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
    Remote {
        _dir: dir,
        store,
        base: format!("http://{addr}"),
        ct,
        work,
        secret,
        glob,
    }
}

#[tokio::test]
async fn a_remote_token_uses_resources_only_as_its_own_agent_and_projects() {
    let srv = remote().await;
    // alice registers a shared dev server locally.
    let local = in_process(srv.store.clone()).await;
    ok(
        &local,
        "resource_register",
        json!({ "agent": "alice", "kind": "dev_server", "name": "vite@5173", "project": srv.work }),
    )
    .await;

    let token = srv
        .store
        .create_token("grok-bot", std::slice::from_ref(&srv.glob), None, None)
        .unwrap();
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("{}/mcp", srv.base))
            .auth_header(token.secret),
    );
    let client: Client = ClientConfig::default().serve(transport).await.unwrap();

    // Finding is open; the acting agent is filled in for the rest.
    let found = call(&client, "resource_find", json!({ "query": "vite" })).await;
    assert_eq!(code(&found), None);
    let b = call(
        &client,
        "resource_borrow",
        json!({ "resource": "dev_server:vite@5173" }),
    )
    .await;
    assert_eq!(code(&b), None, "{b:?}");
    assert_eq!(b.structured_content.unwrap()["lease"]["agent"], "grok-bot");
    let hb = call(&client, "resource_heartbeat", json!({})).await;
    assert_eq!(code(&hb), None);

    // Acting as another agent is refused on every leasing tool.
    for (tool, args) in [
        (
            "resource_borrow",
            json!({ "agent": "alice", "resource": "dev_server:vite@5173" }),
        ),
        (
            "resource_release",
            json!({ "agent": "alice", "resource": "dev_server:vite@5173", "stopped": true }),
        ),
        ("resource_heartbeat", json!({ "agent": "alice" })),
        (
            "resource_register",
            json!({ "agent": "alice", "kind": "port", "name": "tcp:127.0.0.1:9000" }),
        ),
    ] {
        let res = call(&client, tool, args).await;
        assert_eq!(code(&res).as_deref(), Some("forbidden"), "{tool}");
    }
    // Registering into a project outside the token's globs is refused; inside is fine.
    let outside = call(
        &client,
        "resource_register",
        json!({ "kind": "port", "name": "tcp:127.0.0.1:9001", "project": srv.secret }),
    )
    .await;
    assert_eq!(code(&outside).as_deref(), Some("forbidden"));
    let inside = call(
        &client,
        "resource_register",
        json!({ "kind": "port", "name": "tcp:127.0.0.1:9002", "project": srv.work }),
    )
    .await;
    assert_eq!(code(&inside), None, "{inside:?}");

    // The remote borrower releases its own lease; alice still owns the server.
    let rel = call(
        &client,
        "resource_release",
        json!({ "resource": "dev_server:vite@5173" }),
    )
    .await;
    assert_eq!(code(&rel), None);
    let after = rel.structured_content.unwrap();
    assert_eq!(after["resource"]["owner"], "alice@shop");
    assert_eq!(after["resource"]["refcount"], 1);
}
