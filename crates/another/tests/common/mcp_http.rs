//! Shared loopback HTTP test server and MCP client helpers for `crates/another/tests/`.
#![allow(dead_code)]

use std::net::SocketAddr;

use another::mcp::AnotherMcp;
use another::oauth_http::parse_public_url;
use another::serve::{http_router, HttpOptions};
use another_core::{Program, RegisterAgent, Store};
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub type Client = RunningService<RoleClient, ClientConfig>;

/// A running loopback HTTP server. Dropping it only cancels its token and does not wait for
/// the server task to finish; call `stop().await` when a test needs the listener closed.
pub struct TestServer {
    pub addr: SocketAddr,
    pub base: String,
    pub ct: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl TestServer {
    pub async fn stop(mut self) {
        self.ct.cancel();
        if let Some(task) = self.task.take() {
            task.await.unwrap();
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.ct.cancel();
    }
}

/// Bind `127.0.0.1:0`, configure `HttpOptions`, and serve `http_router` until `ct` is cancelled.
pub async fn start_http(
    store: Store,
    ct: CancellationToken,
    configure: impl FnOnce(SocketAddr, &mut HttpOptions),
) -> TestServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut opts = HttpOptions {
        bound_port: addr.port(),
        ..Default::default()
    };
    configure(addr, &mut opts);
    if opts.bound_port == 0 {
        opts.bound_port = addr.port();
    }
    let app = http_router(store, ct.clone(), &opts);
    let stop = ct.clone();
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { stop.cancelled_owned().await })
        .await
        .unwrap();
    });
    TestServer {
        addr,
        base: format!("http://{addr}"),
        ct,
        task: Some(task),
    }
}

/// Start an authenticated loopback HTTP server (`auth: true`).
pub async fn start_auth_http(store: Store) -> TestServer {
    start_http(store, CancellationToken::new(), |_, opts| {
        opts.auth = true;
    })
    .await
}

/// Start an authenticated loopback HTTP server with OAuth enabled for its own bound origin.
pub async fn start_oauth_http(store: Store) -> TestServer {
    start_http(store, CancellationToken::new(), |addr, opts| {
        opts.auth = true;
        opts.oauth = Some(parse_public_url(&format!("http://{addr}")).unwrap());
    })
    .await
}

/// Shared remote/OAuth workspace fixture with two projects (`work/shop`, `secret/vault`) and
/// standard agents (`grok-bot`, `alice@shop`, plus any `extra_agents`).
///
/// Field declaration order ensures `server` cancels and shuts down before `store` and `dir` drop.
pub struct RemoteServer {
    pub server: TestServer,
    pub store: Store,
    pub dir: tempfile::TempDir,
    pub work: String,
    pub secret: String,
    pub work_glob: String,
}

impl RemoteServer {
    pub fn base(&self) -> &str {
        &self.server.base
    }

    pub fn addr(&self) -> SocketAddr {
        self.server.addr
    }

    pub fn cancel(&self) {
        self.server.ct.cancel();
    }

    pub async fn stop(self) {
        self.server.stop().await;
    }
}

async fn start_fixture(
    with_oauth: bool,
    federation: bool,
    extra_agents: &[(&str, Program, Option<&str>)],
) -> RemoteServer {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let work = dir.path().join("work/shop").to_string_lossy().into_owned();
    let secret = dir
        .path()
        .join("secret/vault")
        .to_string_lossy()
        .into_owned();
    let work_glob = dir
        .path()
        .join("work/*")
        .to_string_lossy()
        .replace('\\', "/");
    store.ensure_project("t", &work).unwrap();
    store.ensure_project("t", &secret).unwrap();
    for &(name, program, project) in extra_agents {
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
    let server = if with_oauth {
        start_oauth_http(store.clone()).await
    } else if federation {
        start_http(store.clone(), CancellationToken::new(), |_, opts| {
            opts.auth = true;
            opts.federation = true;
        })
        .await
    } else {
        start_auth_http(store.clone()).await
    };
    RemoteServer {
        server,
        store,
        dir,
        work,
        secret,
        work_glob,
    }
}

/// Start the shared remote fixture (`grok-bot`, `alice@shop`, `auth: true`).
pub async fn start_remote() -> RemoteServer {
    start_fixture(false, false, &[]).await
}

/// Start the shared remote fixture on a listener that also serves federation (`auth: true`).
pub async fn start_remote_federated() -> RemoteServer {
    start_fixture(false, true, &[]).await
}

/// Start the shared OAuth fixture (`chatgpt`, `grok-bot`, `alice@shop`, `auth: true`, OAuth on).
pub async fn start_oauth() -> RemoteServer {
    start_fixture(true, false, &[("chatgpt", Program::Other, None)]).await
}

/// Connect an `rmcp` streamable HTTP client with a bearer token.
pub async fn connect(base: &str, token: &str) -> Client {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("{base}/mcp")).auth_header(token),
    );
    ClientConfig::default().serve(transport).await.unwrap()
}

/// Connect an `rmcp` streamable HTTP client without an authorization header.
pub async fn connect_unauth(base: &str) -> Client {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("{base}/mcp")),
    );
    ClientConfig::default().serve(transport).await.unwrap()
}

/// Connect an in-process `rmcp` client over a 64 KiB duplex stream.
pub async fn in_process(store: Store) -> Client {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let server = AnotherMcp::new(store).serve(server_io).await.unwrap();
        let _ = server.waiting().await;
    });
    ClientConfig::default().serve(client_io).await.unwrap()
}

/// Call an MCP tool with JSON object arguments.
pub async fn call(client: &Client, tool: &str, args: Value) -> CallToolResult {
    let args = args.as_object().cloned().expect("object args");
    client
        .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(args))
        .await
        .expect("tool call")
}

/// Call an MCP tool, assert it succeeded, and return its `structured_content`.
pub async fn call_ok(client: &Client, tool: &str, args: Value) -> Value {
    let res = call(client, tool, args).await;
    assert_ne!(res.is_error, Some(true), "{tool} failed: {res:?}");
    res.structured_content.expect("structured output")
}

/// Extract the `{"code", "message", ...}` error object from a failed `CallToolResult`.
pub fn error_body(res: &CallToolResult) -> Option<Value> {
    if res.is_error != Some(true) {
        return None;
    }
    let text = serde_json::to_value(&res.content).unwrap()[0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    let v: Value = serde_json::from_str(&text).unwrap();
    Some(v["error"].clone())
}

/// Extract `error.code` from a failed `CallToolResult`, or `None` when `is_error != Some(true)`.
pub fn error_code(res: &CallToolResult) -> Option<String> {
    error_body(res).and_then(|e| e["code"].as_str().map(str::to_string))
}

/// Call an MCP tool, assert it failed, and return its `error` JSON object.
pub async fn call_error(client: &Client, tool: &str, args: Value) -> Value {
    let res = call(client, tool, args).await;
    assert_eq!(res.is_error, Some(true), "{tool} should fail: {res:?}");
    error_body(&res).expect("error body")
}

/// Call an MCP tool, assert it failed, and return `error.code`.
pub async fn call_err(client: &Client, tool: &str, args: Value) -> String {
    call_error(client, tool, args).await["code"]
        .as_str()
        .unwrap()
        .to_string()
}
