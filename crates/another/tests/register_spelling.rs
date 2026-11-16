//! A project-scoped token registers its own agent by any spelling of its own project: the
//! stored path, a trailing separator, `./`, `..`, and on Windows the native
//! `C:\...` form (FINDINGS-main-register-spelling; `authorize` used to compare the raw
//! argument with the stored path). Another project inside the token's globs, or a directory
//! that is no project, is still refused.

use std::net::SocketAddr;

use another::serve::{http_router, HttpOptions};
use another_core::{Program, RegisterAgent, Store};
use rmcp::model::{CallToolRequestParams, ClientConfig};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::ServiceExt;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn a_token_registers_its_agent_by_any_spelling_of_its_own_project() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let work = dir.path().join("work").join("shop");
    let work = work.to_string_lossy().into_owned();
    store.ensure_project("t", &work).unwrap();
    let other = dir.path().join("work").join("other");
    let other = other.to_string_lossy().into_owned();
    store.ensure_project("t", &other).unwrap();
    let nowhere = dir.path().join("work").join("nowhere");
    let nowhere = nowhere.to_string_lossy().into_owned();
    store
        .register_agent(RegisterAgent {
            name: "alice".into(),
            program: Program::Claude,
            project: Some(work.clone()),
            model: None,
            description: None,
        })
        .unwrap();
    let glob = dir
        .path()
        .join("work")
        .join("*")
        .to_string_lossy()
        .replace('\\', "/");
    let token = store
        .create_token("alice@shop", &[glob], None, None)
        .unwrap();

    let ct = CancellationToken::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let opts = HttpOptions {
        auth: true,
        bound_port: addr.port(),
        ..Default::default()
    };
    let app = http_router(store.clone(), ct.clone(), &opts);
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let client = ClientConfig::default()
        .serve(StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp"))
                .auth_header(&token.secret),
        ))
        .await
        .unwrap();

    let register = |project: String| {
        let client = &client;
        async move {
            let res = client
                .call_tool(
                    CallToolRequestParams::new("register_agent").with_arguments(
                        json!({ "name": "alice", "program": "claude", "project": project })
                            .as_object()
                            .cloned()
                            .unwrap(),
                    ),
                )
                .await
                .unwrap();
            let text = serde_json::to_value(&res.content).unwrap()[0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            (res.is_error == Some(true), body)
        }
    };

    let sep = std::path::MAIN_SEPARATOR;
    for project in [
        work.clone(),
        format!("{work}{sep}"),
        format!("{work}{sep}.{sep}"),
        format!("{work}{sep}..{sep}shop"),
    ] {
        let (failed, body) = register(project.clone()).await;
        assert!(!failed, "{project:?} is shop itself: {body}");
    }
    for project in [
        other.clone(),
        format!("{other}{sep}"),
        "other".into(),
        nowhere,
    ] {
        let (failed, body) = register(project.clone()).await;
        assert!(
            failed,
            "{project:?} is not shop, yet it was accepted: {body}"
        );
        assert_eq!(body["error"]["code"], "forbidden", "{project:?}: {body}");
    }
    ct.cancel();
}
