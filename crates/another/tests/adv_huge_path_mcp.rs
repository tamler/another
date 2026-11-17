//! Adversarial (main): a very long path through MCP `hold_paths` panics in globset. The
//! server must answer with an `invalid_argument`-style error, not a panic, and keep serving.

use another::mcp::AnotherMcp;
use another_core::Store;
use rmcp::model::{CallToolRequestParams, ClientConfig};
use rmcp::ServiceExt;
use serde_json::{json, Value};

#[tokio::test]
async fn gap_mcp_hold_paths_with_a_huge_path_is_a_clean_error() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("home")).unwrap();
    let project = dir.path().join("shop").to_string_lossy().into_owned();
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let server = AnotherMcp::new(store).serve(server_io).await.unwrap();
        let _ = server.waiting().await;
    });
    let client = ClientConfig::default().serve(client_io).await.unwrap();
    let call = |tool: &str, args: Value| {
        client.call_tool(
            CallToolRequestParams::new(tool.to_string())
                .with_arguments(args.as_object().cloned().unwrap()),
        )
    };
    call(
        "register_agent",
        json!({ "name": "bob", "program": "other", "project": project }),
    )
    .await
    .unwrap();
    let huge = format!("src/{}.rs", "a".repeat(1_000_000));
    let res = call(
        "hold_paths",
        json!({ "agent": "bob", "project": project, "paths": [huge] }),
    )
    .await
    .unwrap();
    let text = serde_json::to_value(&res.content).unwrap()[0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    eprintln!(
        "ADV mcp is_error={:?} body={}",
        res.is_error,
        &text[..text.len().min(300)]
    );
    // The server is still up.
    let again = call(
        "hold_paths",
        json!({ "agent": "bob", "project": project, "paths": ["src/ok.rs"] }),
    )
    .await
    .unwrap();
    assert_ne!(again.is_error, Some(true), "{again:?}");
    assert_eq!(res.is_error, Some(true));
    let code = serde_json::from_str::<Value>(&text).unwrap()["error"]["code"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert_ne!(
        code, "internal",
        "a long path should be refused, not crash a worker: {text}"
    );
}
