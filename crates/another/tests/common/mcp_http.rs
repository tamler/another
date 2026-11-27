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
/// Dropping `RemoteServer` only signals cancellation on `server` before `store` and `dir` drop;
/// the background server task may still hold a `Store` clone until it exits. Call
/// [`RemoteServer::stop`] when deterministic shutdown before `store` and `dir` cleanup matters.
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

    /// Cancel the server and wait for its task to exit before `store` and `dir` drop.
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
///
/// Panics with the full response when `is_error == Some(true)` does not carry a JSON payload
/// with an `"error"` object.
pub fn error_body(res: &CallToolResult) -> Option<Value> {
    if res.is_error != Some(true) {
        return None;
    }
    let content = serde_json::to_value(&res.content)
        .unwrap_or_else(|err| panic!("failed to serialize MCP error content ({err}): {res:?}"));
    let text = content
        .get(0)
        .and_then(|item| item.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("MCP error result missing text content: {res:?}"));
    let v: Value = serde_json::from_str(text)
        .unwrap_or_else(|err| panic!("MCP error result content is not JSON ({err}): {res:?}"));
    let error = v
        .get("error")
        .filter(|e| e.is_object())
        .unwrap_or_else(|| panic!("MCP error result missing error object: {res:?}"));
    Some(error.clone())
}

/// Extract `error.code` from a failed `CallToolResult`, or `None` when `is_error != Some(true)`.
///
/// Panics with the full response when `is_error == Some(true)` does not carry a non-empty string
/// `error.code`, so `assert_eq!(error_code(&res), None)` can never pass on a failed call.
pub fn error_code(res: &CallToolResult) -> Option<String> {
    let error = error_body(res)?;
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .filter(|code| !code.is_empty())
        .unwrap_or_else(|| panic!("MCP error result missing string error.code: {res:?}"));
    Some(code.to_string())
}

/// Call an MCP tool, assert it failed, and return its `error` JSON object.
pub async fn call_error(client: &Client, tool: &str, args: Value) -> Value {
    let res = call(client, tool, args).await;
    assert_eq!(res.is_error, Some(true), "{tool} should fail: {res:?}");
    error_body(&res).expect("error body")
}

/// Call an MCP tool, assert it failed, and return `error.code`.
pub async fn call_err(client: &Client, tool: &str, args: Value) -> String {
    let res = call(client, tool, args).await;
    assert_eq!(res.is_error, Some(true), "{tool} should fail: {res:?}");
    error_code(&res).expect("error_code must return Some when is_error is Some(true)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::ContentBlock;

    fn panic_message(f: impl FnOnce() + std::panic::UnwindSafe) -> String {
        let payload = std::panic::catch_unwind(f).expect_err("expected panic");
        if let Some(msg) = payload.downcast_ref::<String>() {
            msg.clone()
        } else if let Some(msg) = payload.downcast_ref::<&str>() {
            (*msg).to_string()
        } else {
            panic!("unexpected non-string panic payload");
        }
    }

    #[test]
    fn error_code_returns_none_for_success_and_code_for_valid_error() {
        let ok_none = CallToolResult::success(vec![]);
        assert_eq!(error_code(&ok_none), None);

        let mut ok_false = CallToolResult::success(vec![]);
        ok_false.is_error = Some(false);
        assert_eq!(error_code(&ok_false), None);

        let err = CallToolResult::error(vec![ContentBlock::text(
            r#"{"error":{"code":"forbidden","message":"denied"}}"#,
        )]);
        assert_eq!(error_code(&err).as_deref(), Some("forbidden"));
    }

    #[test]
    fn error_code_panics_with_payload_when_is_error_lacks_string_code() {
        for (body, marker) in [
            (
                r#"{"error":{"message":"missing-code-marker"}}"#,
                "missing-code-marker",
            ),
            (
                r#"{"error":{"code":403,"message":"numeric-code-marker"}}"#,
                "numeric-code-marker",
            ),
            (
                r#"{"error":{"code":null,"message":"null-code-marker"}}"#,
                "null-code-marker",
            ),
            (
                r#"{"error":{"code":"","message":"empty-code-marker"}}"#,
                "empty-code-marker",
            ),
            (
                r#"{"message":"missing-error-object-marker"}"#,
                "missing-error-object-marker",
            ),
        ] {
            let res = CallToolResult::error(vec![ContentBlock::text(body)]);
            let msg = panic_message(std::panic::AssertUnwindSafe(|| {
                let _ = error_code(&res);
            }));
            assert!(
                msg.contains(marker),
                "panic message should include response payload ({marker}): {msg}"
            );
        }
    }
}
