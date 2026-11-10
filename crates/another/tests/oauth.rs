//! OAuth 2.1 for hosted MCP clients: discovery, PKCE, local approval, refresh, revocation.
//!
//! The MCP half uses the `rmcp` client over the access token the authorization-code flow
//! just issued. Approval goes through the store, the same call `another oauth approve` makes.
//! There is no HTTP request that can approve a client.

use std::net::SocketAddr;

use another::oauth_http::parse_public_url;
use another::serve::{http_router, HttpOptions};
use another_core::{Approval, Program, RegisterAgent, SendMessage, Store};
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

type Client = RunningService<RoleClient, ClientConfig>;

const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

struct Server {
    store: Store,
    base: String,
    addr: SocketAddr,
    ct: CancellationToken,
}

async fn start() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    // Keep the temp dir alive by leaking it for the process. The test is short.
    std::mem::forget(dir);
    store.ensure_project("t", "/work/shop").unwrap();
    store.ensure_project("t", "/secret/vault").unwrap();
    for (name, program, project) in [
        ("chatgpt", Program::Other, None),
        ("grok-bot", Program::GrokBot, None),
        ("alice", Program::Claude, Some("/work/shop")),
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let ct = CancellationToken::new();
    let opts = HttpOptions {
        auth: true,
        oauth: Some(parse_public_url(&base).unwrap()),
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
        store,
        base,
        addr,
        ct,
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

fn with_query(url: &str, pairs: &[(&str, &str)]) -> String {
    format!("{url}?{}", form(pairs))
}

fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn query_param(url: &str, key: &str) -> String {
    url.split(&format!("{key}="))
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap()
        .to_string()
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

async fn raw_status(addr: SocketAddr, host: &str, path: &str) -> u16 {
    let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    sock.write_all(req.as_bytes()).await.unwrap();
    let mut buf = vec![0u8; 256];
    let n = sock.read(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf[..n]);
    text.split_whitespace().nth(1).unwrap().parse().unwrap()
}

#[tokio::test]
async fn oauth_pkce_then_mcp_client_and_bearer_still_works() {
    let srv = start().await;
    let http = http();

    let prm: Value = http
        .get(format!(
            "{}/.well-known/oauth-protected-resource/mcp",
            srv.base
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(prm["resource"], format!("{}/mcp", srv.base));
    assert_eq!(prm["authorization_servers"][0], srv.base);
    assert_eq!(prm["scopes_supported"][0], "another");

    let asm: Value = http
        .get(format!(
            "{}/.well-known/oauth-authorization-server",
            srv.base
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(asm["issuer"], srv.base);
    assert_eq!(asm["authorization_response_iss_parameter_supported"], true);
    assert_eq!(asm["client_id_metadata_document_supported"], true);
    assert!(asm["code_challenge_methods_supported"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "S256"));
    assert!(asm["token_endpoint_auth_methods_supported"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "none"));
    assert!(asm["registration_endpoint"]
        .as_str()
        .unwrap()
        .ends_with("/oauth/register"));

    let oidc: Value = http
        .get(format!("{}/.well-known/openid-configuration", srv.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(oidc["issuer"], srv.base);
    assert!(oidc["code_challenge_methods_supported"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "S256"));

    let unauth = http.get(format!("{}/mcp", srv.base)).send().await.unwrap();
    assert_eq!(unauth.status(), 401);
    let www = unauth
        .headers()
        .get("www-authenticate")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(www.contains("resource_metadata="));
    assert!(www.contains("/.well-known/oauth-protected-resource/mcp"));
    assert!(www.contains("scope=\"another\""));

    assert_eq!(
        raw_status(
            srv.addr,
            "evil.example",
            "/.well-known/oauth-authorization-server"
        )
        .await,
        403
    );
    assert_eq!(
        raw_status(srv.addr, &format!("{}", srv.addr), "/health").await,
        200
    );

    let registered: Value = http
        .post(format!("{}/oauth/register", srv.base))
        .json(&json!({
            "client_name": "ChatGPT",
            "redirect_uris": ["https://chatgpt.com/connector_platform_oauth_redirect"],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
            "application_type": "web"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let client_id = registered["client_id"].as_str().unwrap().to_string();

    // A web client may not register a loopback redirect (OIDC default).
    let native_rejected = http
        .post(format!("{}/oauth/register", srv.base))
        .json(&json!({
            "client_name": "loop",
            "redirect_uris": ["http://127.0.0.1/callback"],
            "application_type": "web"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(native_rejected.status(), 400);

    let redirect = "https://chatgpt.com/connector_platform_oauth_redirect";
    let auth = http
        .get(with_query(
            &format!("{}/authorize", srv.base),
            &[
                ("response_type", "code"),
                ("client_id", client_id.as_str()),
                ("redirect_uri", redirect),
                ("code_challenge", CHALLENGE),
                ("code_challenge_method", "S256"),
                ("resource", &format!("{}/mcp", srv.base)),
                ("state", "from-the-app"),
                ("scope", "another offline_access"),
            ],
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(auth.status(), 303);
    let wait = auth.headers().get("location").unwrap().to_str().unwrap();
    assert!(wait.starts_with("/oauth/wait/"));
    assert!(wait.contains("rt="));
    let page = http
        .get(format!("{}{wait}", srv.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("cannot grant"));
    assert!(!page.to_ascii_lowercase().contains("<form"));

    let pending_url = format!(
        "{}{}",
        srv.base,
        wait.replacen("/oauth/wait/", "/oauth/pending/", 1)
    );
    let pending: Value = http
        .get(&pending_url)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(pending["status"], "pending");
    assert!(pending["redirect"].is_null());
    let user_code = pending["user_code"].as_str().unwrap().to_string();

    // Nothing on the HTTP API approves a client.
    let no_approve = http
        .post(format!("{}/oauth/approve", srv.base))
        .json(&json!({"user_code": user_code}))
        .send()
        .await
        .unwrap();
    assert_eq!(no_approve.status(), 404);

    srv.store
        .approve_oauth(Approval {
            user_code,
            agent: "chatgpt".into(),
            project_globs: vec!["/work/*".into()],
            label: Some("chatgpt".into()),
        })
        .unwrap();
    let decided: Value = http
        .get(&pending_url)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let back = decided["redirect"].as_str().unwrap();
    assert!(back.starts_with(redirect));
    assert!(back.contains("iss="));
    assert!(back.contains("state=from-the-app"));
    let code = query_param(back, "code");

    // JSON is not accepted at the token endpoint.
    let json_token = http
        .post(format!("{}/oauth/token", srv.base))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(json_token.status(), 415);

    let wrong = http
        .post(format!("{}/oauth/token", srv.base))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", redirect),
            ("client_id", &client_id),
            (
                "code_verifier",
                "wrong-verifier-wrong-verifier-wrong-verifier-wrong",
            ),
            ("resource", &format!("{}/mcp", srv.base)),
        ]))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 400);
    assert_eq!(
        wrong.json::<Value>().await.unwrap()["error"],
        "invalid_grant"
    );

    let token_res: Value = http
        .post(format!("{}/oauth/token", srv.base))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form(&[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", redirect),
            ("client_id", &client_id),
            ("code_verifier", VERIFIER),
            ("resource", &format!("{}/mcp", srv.base)),
        ]))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let access = token_res["access_token"].as_str().unwrap().to_string();
    let refresh = token_res["refresh_token"].as_str().unwrap().to_string();
    assert_eq!(token_res["token_type"], "Bearer");
    assert_eq!(token_res["expires_in"], 3600);

    srv.store
        .send_message(SendMessage {
            from: "alice".into(),
            to: vec!["chatgpt".into()],
            cc: vec![],
            subject: "review the migration".into(),
            body: "secret body that the digest must not return".into(),
            importance: Default::default(),
            ack_required: false,
            reply_to: None,
            thread_id: None,
            request: None,
        })
        .unwrap();

    let resource = format!("{}/mcp", srv.base);
    srv.store
        .verify_access_token(&access, &resource)
        .expect("access token should verify");
    srv.store
        .verify_access_token(&access, &resource)
        .expect("access token should verify twice");
    let client = connect(&srv.base, &access).await;
    let digest = call(&client, "whats_new", json!({})).await;
    assert_eq!(error_code(&digest), None);
    let body = digest.structured_content.unwrap();
    assert_eq!(body["agent"], "chatgpt");
    assert_eq!(body["unread"], 1);
    assert_eq!(body["messages"][0]["subject"], "review the migration");
    assert!(body["messages"][0].get("body").is_none());
    let rendered = body.to_string();
    assert!(!rendered.contains("secret body"));
    assert!(body["next"].as_str().unwrap().contains("unread"));

    let outside = call(&client, "ensure_project", json!({"path": "/secret/vault"})).await;
    assert_eq!(error_code(&outside).as_deref(), Some("forbidden"));
    let as_alice = call(&client, "fetch_inbox", json!({"agent": "alice"})).await;
    assert_eq!(error_code(&as_alice).as_deref(), Some("forbidden"));

    // Bearer tokens keep working on the same server.
    let bearer = srv
        .store
        .create_token("grok-bot", &["/work/*".into()], Some("bot".into()), None)
        .unwrap();
    let bot = connect(&srv.base, &bearer.secret).await;
    let inbox = call(&bot, "fetch_inbox", json!({})).await;
    assert_eq!(error_code(&inbox), None);
    assert_eq!(inbox.structured_content.unwrap()["agent"], "grok-bot");

    let rotated: Value = http
        .post(format!("{}/oauth/token", srv.base))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh),
            ("client_id", &client_id),
            ("resource", &format!("{}/mcp", srv.base)),
        ]))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let new_refresh = rotated["refresh_token"].as_str().unwrap().to_string();
    assert_ne!(new_refresh, refresh);

    let reuse = http
        .post(format!("{}/oauth/token", srv.base))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh),
            ("client_id", &client_id),
            ("resource", &format!("{}/mcp", srv.base)),
        ]))
        .send()
        .await
        .unwrap();
    assert_eq!(reuse.status(), 400);
    // Reuse revoked the grant, so the rotated refresh is dead too.
    let after = http
        .post(format!("{}/oauth/token", srv.base))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", &new_refresh),
            ("client_id", &client_id),
            ("resource", &format!("{}/mcp", srv.base)),
        ]))
        .send()
        .await
        .unwrap();
    assert_eq!(after.status(), 400);

    // The old access token was not the thing that got reused; the grant revoke kills it.
    let denied = http
        .post(format!("{}/mcp", srv.base))
        .header("authorization", format!("Bearer {access}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);

    let audit = serde_json::to_string(&srv.store.audit_log(100).unwrap()).unwrap();
    assert!(!audit.contains(&access));
    assert!(!audit.contains(&refresh));
    assert!(!audit.contains(&new_refresh));
    assert!(!audit.contains(VERIFIER));
    assert!(!audit.contains("secret body"));

    srv.ct.cancel();
}

#[tokio::test]
async fn cimd_and_loopback_redirect_and_error_iss() {
    let srv = start().await;
    let http = http();

    // Client metadata hosted on loopback, allowed only because this server's own origin is loopback.
    let doc_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let doc_addr = doc_listener.local_addr().unwrap();
    let client_id = format!("http://{doc_addr}/client.json");
    let doc = json!({
        "client_id": client_id,
        "client_name": "Claude",
        "redirect_uris": [
            "https://claude.ai/api/mcp/auth_callback",
            "http://127.0.0.1/callback",
            "http://localhost/callback"
        ],
        "token_endpoint_auth_method": "none"
    })
    .to_string();
    tokio::spawn(async move {
        let (mut sock, _) = doc_listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let _ = sock.read(&mut buf).await;
        let body = doc.as_bytes();
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        let _ = sock.write_all(resp.as_bytes()).await;
        let _ = sock.write_all(body).await;
    });

    let bad = http
        .get(with_query(
            &format!("{}/authorize", srv.base),
            &[
                ("response_type", "code"),
                ("client_id", client_id.as_str()),
                ("redirect_uri", "https://evil.example/steal"),
                ("code_challenge", CHALLENGE),
                ("code_challenge_method", "S256"),
                ("resource", &format!("{}/mcp", srv.base)),
            ],
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
    assert!(bad.headers().get("location").is_none());

    let ok = http
        .get(with_query(
            &format!("{}/authorize", srv.base),
            &[
                ("response_type", "code"),
                ("client_id", client_id.as_str()),
                ("redirect_uri", "http://127.0.0.1:3118/callback"),
                ("code_challenge", CHALLENGE),
                ("code_challenge_method", "S256"),
                ("resource", &format!("{}/mcp", srv.base)),
                ("state", "s"),
            ],
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 303, "{}", ok.text().await.unwrap_or_default());

    // A valid redirect with a missing challenge is an error redirect that still carries iss.
    let err = http
        .get(with_query(
            &format!("{}/authorize", srv.base),
            &[
                ("response_type", "code"),
                ("client_id", client_id.as_str()),
                ("redirect_uri", "https://claude.ai/api/mcp/auth_callback"),
                ("resource", &format!("{}/mcp", srv.base)),
                ("state", "s2"),
            ],
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(err.status(), 303);
    let loc = err.headers().get("location").unwrap().to_str().unwrap();
    assert!(loc.starts_with("https://claude.ai/api/mcp/auth_callback"));
    assert!(loc.contains("error=invalid_request"));
    assert!(loc.contains("iss="));
    assert!(loc.contains("state=s2"));

    srv.ct.cancel();
}
