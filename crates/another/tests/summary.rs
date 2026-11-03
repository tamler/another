//! Daily summary end to end: CLI (markdown and JSON), MCP `daily_summary`, and
//! `GET /summary` as JSON and markdown, scoped and audited. Temporary homes, ephemeral ports.

use std::net::SocketAddr;
use std::process::{Command, Stdio};

use another::serve::{http_router, HttpOptions};
use another_core::{NewLane, Program, RegisterAgent, Store};
use rmcp::model::{CallToolRequestParams, ClientConfig};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::ServiceExt;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

#[test]
fn summary_cli() {
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_another"))
            .args(args)
            .env("ANOTHER_HOME", dir.path())
            .env("HOME", dir.path())
            .env_remove("ANOTHER_AGENT")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    let md = run(&["summary"]);
    assert!(
        md.starts_with("**Another: last 24h**\nNothing needs you."),
        "{md}"
    );
    let j: Value = serde_json::from_str(&run(&["--json", "summary", "--since", "2h"])).unwrap();
    assert!(j["markdown"]
        .as_str()
        .unwrap()
        .starts_with("**Another: last 2h**"));
    assert!(j["attention"].as_array().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn summary_over_mcp_and_http_is_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    for p in ["/work/shop", "/work/vault"] {
        store.ensure_project("t", p).unwrap();
    }
    for (n, p) in [
        ("manager", None),
        ("claude", Some("/work/shop")),
        ("vaulter", Some("/work/vault")),
    ] {
        store
            .register_agent(RegisterAgent {
                name: n.into(),
                program: Program::Claude,
                project: p.map(Into::into),
                model: None,
                description: None,
            })
            .unwrap();
    }
    for (lane, project, lead) in [
        ("api", "shop", "claude@shop"),
        ("vault-keys", "vault", "vaulter@vault"),
    ] {
        store
            .create_lane(
                NewLane {
                    agent: "manager".into(),
                    name: lane.into(),
                    project: project.into(),
                    lead: lead.into(),
                    charter: String::new(),
                    status: Some(another_core::LaneStatus::Blocked),
                },
                None,
            )
            .unwrap();
    }
    let token = store
        .create_token("manager", &["/work/shop".into()], None, None)
        .unwrap()
        .secret;
    let ct = CancellationToken::new();
    let app = http_router(
        store.clone(),
        ct.clone(),
        &HttpOptions {
            auth: true,
            ..Default::default()
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    assert_ne!(addr.port(), 7466);
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
    let base = format!("http://{addr}");

    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("{base}/mcp"))
            .auth_header(token.clone()),
    );
    let client = ClientConfig::default().serve(transport).await.unwrap();
    let r = client
        .call_tool(
            CallToolRequestParams::new("daily_summary")
                .with_arguments(json!({ "since": "24h" }).as_object().cloned().unwrap()),
        )
        .await
        .unwrap();
    let text = serde_json::to_value(&r.content).unwrap()[0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        v["attention"],
        json!(["Lane api (blocked, lead claude@shop)"])
    );
    assert!(!text.contains("vault"), "{text}");

    let http = reqwest::Client::new();
    assert_eq!(
        http.get(format!("{base}/summary"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let md = http
        .get(format!("{base}/summary?format=markdown"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(md.status(), 200);
    assert!(md.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/markdown"));
    let md = md.text().await.unwrap();
    assert!(md.contains("- Lane api (blocked"), "{md}");
    assert!(!md.contains("vault"), "{md}");
    let j: Value = http
        .get(format!("{base}/summary?since=1h"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(j["lanes"]["open"]["blocked"], 1);

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let ops: Vec<(String, String)> = store
        .audit_log(50)
        .unwrap()
        .into_iter()
        .map(|a| (a.operation, a.outcome))
        .collect();
    assert!(ops.contains(&("GET /summary".into(), "ok".into())));
    assert!(ops.contains(&("GET /summary".into(), "denied".into())));
    assert!(ops.contains(&("tools/call daily_summary".into(), "ok".into())));
    client.cancel().await.unwrap();
    ct.cancel();
}
