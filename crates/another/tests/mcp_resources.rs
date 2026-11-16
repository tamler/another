//! The resource tools over MCP map their arguments onto the store as the CLI does:
//! `exclusive` makes a lease exclusive, `ttl_seconds` sets the lease TTL, a resource is named
//! by id or `kind:name`, and `stopped` is for the owner only. No test called
//! `resource_borrow` or `resource_release` through MCP.

use another::mcp::AnotherMcp;
use another_core::{Program, RegisterAgent, Store, DB_FILE};
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};

type Client = RunningService<RoleClient, ClientConfig>;

async fn call(client: &Client, tool: &str, args: Value) -> CallToolResult {
    let args = args.as_object().cloned().expect("object args");
    client
        .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(args))
        .await
        .expect("tool call")
}

async fn ok(client: &Client, tool: &str, args: Value) -> Value {
    let res = call(client, tool, args).await;
    assert_ne!(res.is_error, Some(true), "{tool} failed: {res:?}");
    res.structured_content.expect("structured output")
}

/// The error code and message of a failed call.
async fn err(client: &Client, tool: &str, args: Value) -> (String, String) {
    let res = call(client, tool, args).await;
    assert_eq!(res.is_error, Some(true), "{tool} should fail: {res:?}");
    let text = serde_json::to_value(&res.content).unwrap()[0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    let body: Value = serde_json::from_str(&text).unwrap();
    (
        body["error"]["code"].as_str().unwrap().to_string(),
        body["error"]["message"].as_str().unwrap().to_string(),
    )
}

struct Fx {
    _dir: tempfile::TempDir,
    db: std::path::PathBuf,
    client: Client,
}

async fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let store = Store::open(&home).unwrap();
    for name in ["owner", "borrower"] {
        store
            .register_agent(RegisterAgent {
                name: name.into(),
                program: Program::Claude,
                project: None,
                model: None,
                description: None,
            })
            .unwrap();
    }
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let server = AnotherMcp::new(store).serve(server_io).await.unwrap();
        let _ = server.waiting().await;
    });
    Fx {
        _dir: dir,
        db: home.join(DB_FILE),
        client: ClientConfig::default().serve(client_io).await.unwrap(),
    }
}

impl Fx {
    async fn register(&self, name: &str, exclusive: bool) -> String {
        let out = ok(
            &self.client,
            "resource_register",
            json!({ "agent": "owner", "kind": "model_server", "name": name,
                    "exclusive": exclusive, "ttl_seconds": 600 }),
        )
        .await;
        assert_eq!(out["outcome"], "registered", "{out}");
        out["resource"]["resource_id"].as_str().unwrap().to_string()
    }

    fn lease_ttl(&self, lease_id: &str) -> i64 {
        rusqlite::Connection::open(&self.db)
            .unwrap()
            .query_row(
                "SELECT ttl_seconds FROM resource_leases WHERE lease_id = ?1",
                [lease_id],
                |r| r.get(0),
            )
            .unwrap()
    }
}

#[tokio::test]
async fn exclusive_and_ttl_reach_the_lease() {
    let f = fx().await;
    f.register("solo@1", true).await;
    let (code, message) = err(
        &f.client,
        "resource_borrow",
        json!({ "agent": "borrower", "resource": "model_server:solo@1" }),
    )
    .await;
    assert_eq!(code, "conflict", "{message}");
    assert!(message.contains("exclusive"), "{message}");

    let shared = f.register("shared@2", false).await;
    let out = ok(
        &f.client,
        "resource_borrow",
        json!({ "agent": "borrower", "resource": shared, "ttl_seconds": 120 }),
    )
    .await;
    assert_eq!(out["outcome"], "borrowed", "{out}");
    assert_eq!(out["lease"]["role"], "borrower", "{out}");
    let lease = out["lease"]["lease_id"].as_str().unwrap();
    assert_eq!(f.lease_ttl(lease), 120);

    // The same resource by `kind:name`: borrowing again renews the same lease.
    let again = ok(
        &f.client,
        "resource_borrow",
        json!({ "agent": "borrower", "resource": "model_server:shared@2", "ttl_seconds": 300 }),
    )
    .await;
    assert_eq!(again["outcome"], "renewed", "{again}");
    assert_eq!(again["lease"]["lease_id"].as_str().unwrap(), lease);
    assert_eq!(f.lease_ttl(lease), 300);

    let (code, _) = err(
        &f.client,
        "resource_borrow",
        json!({ "agent": "borrower", "resource": "01NOSUCHRESOURCE" }),
    )
    .await;
    assert_eq!(code, "not_found");
}

#[tokio::test]
async fn only_the_owner_may_release_with_stopped() {
    let f = fx().await;
    let id = f.register("svc@3", false).await;
    ok(
        &f.client,
        "resource_borrow",
        json!({ "agent": "borrower", "resource": id }),
    )
    .await;

    let (code, message) = err(
        &f.client,
        "resource_release",
        json!({ "agent": "borrower", "resource": id, "stopped": true }),
    )
    .await;
    assert_eq!(code, "forbidden", "{message}");
    assert!(message.contains("only the owner"), "{message}");

    let released = ok(
        &f.client,
        "resource_release",
        json!({ "agent": "borrower", "resource": id }),
    )
    .await;
    assert_eq!(released["outcome"], "released", "{released}");
    assert!(released["lease"].is_null(), "{released}");

    let stopped = ok(
        &f.client,
        "resource_release",
        json!({ "agent": "owner", "resource": id, "stopped": true }),
    )
    .await;
    assert_eq!(stopped["outcome"], "stopped", "{stopped}");
    assert_eq!(stopped["resource"]["status"], "released", "{stopped}");
}

/// adv227: `resource_register` passes its TTL, endpoint, port and metadata to the store.
#[tokio::test]
async fn adv227_register_keeps_ttl_endpoint_port_and_metadata() {
    let f = fx().await;
    let out = ok(
        &f.client,
        "resource_register",
        json!({ "agent": "owner", "kind": "model_server", "name": "ollama@11434",
                "endpoint": "http://127.0.0.1:11434", "port": 11434,
                "metadata": { "model": "llama3" }, "ttl_seconds": 600 }),
    )
    .await;
    let res = &out["resource"];
    assert_eq!(res["endpoint"], "http://127.0.0.1:11434", "{out}");
    assert_eq!(res["port"], 11434, "{out}");
    assert_eq!(res["metadata"]["model"], "llama3", "{out}");
    let lease = out["lease"]["lease_id"].as_str().unwrap();
    assert_eq!(f.lease_ttl(lease), 600);
}

/// adv227: `resource_heartbeat` renews only the resources it names, and all of the
/// caller's leases when it names none.
#[tokio::test]
async fn adv227_heartbeat_renews_only_the_named_resources() {
    let f = fx().await;
    let a = f.register("a@1", false).await;
    let b = f.register("b@2", false).await;
    let expiry = |id: &str| -> i64 {
        rusqlite::Connection::open(&f.db)
            .unwrap()
            .query_row(
                "SELECT expires_at FROM resource_leases WHERE resource_id = ?1 AND role = 'owner'",
                [id],
                |r| r.get(0),
            )
            .unwrap()
    };
    let (a0, b0) = (expiry(&a), expiry(&b));
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    ok(
        &f.client,
        "resource_heartbeat",
        json!({ "agent": "owner", "resources": [a] }),
    )
    .await;
    assert!(expiry(&a) >= a0 + 1_000, "the named lease was not renewed");
    assert_eq!(expiry(&b), b0, "an unnamed lease was renewed");

    let a1 = expiry(&a);
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;
    ok(&f.client, "resource_heartbeat", json!({ "agent": "owner" })).await;
    assert!(expiry(&a) >= a1 + 1_000 && expiry(&b) >= b0 + 1_000);
}
