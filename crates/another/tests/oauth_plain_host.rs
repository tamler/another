//! Q28 (D100): registration refuses a redirect URI whose host is not plain, through the
//! real HTTP routes of a server on loopback in a temp home. No request leaves loopback, no
//! redirect is followed, and no test host name is ever contacted.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use another::oauth_http::parse_public_url;
use another::serve::{http_router, HttpOptions};
use another_core::{Approval, Program, RegisterAgent, Store, DB_FILE};
use rmcp::model::{CallToolRequestParams, ClientConfig};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::ServiceExt;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const REFUSED: &str = "redirect URI is not allowed: it must be an https or http URI with no white space, `#` or `@`, and its host must be a plain host name or IP address with an optional port from 1 to 65535. An internationalised name must be given in its punycode form.";
const CLIENT_REFUSED: &str = "this client is registered with a redirect host that is not a plain host name or IP address. It must register again.";

struct Server {
    store: Store,
    dir: tempfile::TempDir,
    base: String,
    ct: CancellationToken,
}

impl Server {
    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.home().join(DB_FILE)).unwrap()
    }

    fn client_rows(&self) -> i64 {
        self.db()
            .query_row("SELECT count(*) FROM oauth_clients", [], |r| r.get(0))
            .unwrap()
    }

    fn request_rows(&self) -> i64 {
        self.db()
            .query_row("SELECT count(*) FROM oauth_requests", [], |r| r.get(0))
            .unwrap()
    }

    /// Store `uris` for a client as a build before D100 would have accepted them.
    fn store_redirects(&self, client_id: &str, uris: &[&str]) {
        let changed = self
            .db()
            .execute(
                "UPDATE oauth_clients SET redirect_uris = ?1 WHERE client_id = ?2",
                rusqlite::params![serde_json::to_string(uris).unwrap(), client_id],
            )
            .unwrap();
        assert_eq!(changed, 1);
    }

    /// The standard output of the built `another` against this server's home.
    fn cli(&self, args: &[&str]) -> String {
        let out = {
            let mut legacy_fixture_command = Command::new(env!("CARGO_BIN_EXE_another"));
            legacy_fixture_command.arg("--unguarded-repair");
            legacy_fixture_command
        }
        .args(args)
        .current_dir(self.dir.path())
        .env("ANOTHER_HOME", self.home())
        .env_remove("ANOTHER_AGENT")
        .env_remove("ANOTHER_LOG")
        .stdin(Stdio::null())
        .output()
        .expect("run another");
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

async fn start() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("home")).unwrap();
    store
        .register_agent(RegisterAgent {
            name: "chatgpt".into(),
            program: Program::Other,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let ct = CancellationToken::new();
    let opts = HttpOptions {
        legacy_repair: true,
        auth: true,
        bound_port: addr.port(),
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
        dir,
        base,
        ct,
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
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

/// The host parts the reviews of pull request 273 found registration to accept, the
/// `+443` port it accepted, and the two ports it refused with another answer.
fn hostile_authorities() -> Vec<String> {
    let mut hosts: Vec<String> = [
        "attacker.test\\good.example.com",
        "auth.openai.com\u{200b}\u{200b}\u{200b}chatgpt.com",
        "attacker.test\u{200b}\u{200b}expires\u{200b}2026-11-20t09.00.00.000z\u{200b}\u{200b}redirect\u{200b}host\u{200b}chatgpt.com",
        "0127.0.0.1",
        "2130706433",
        "0x7f.1",
        "1.2.3",
        "example.com.",
        "[attacker.test]",
        "[a:b:c]",
        "[1::2::3]",
        "[fe80::1%eth0]",
        "b\u{fc}cher.example",
        "\u{430}ttacker.test",
        "chatgpt.com:+443",
        "127.0.0.1:+443",
        "[::1]:+443",
        "localhost:+443",
        "chatgpt.com:0",
        "chatgpt.com:65536",
    ]
    .iter()
    .map(|h| h.to_string())
    .collect();
    hosts.push(format!(
        "attacker.test\\{}.good.example.com",
        "x".repeat(300)
    ));
    hosts.push(format!("a{}.attacker.test", "a".repeat(253 - 14)));
    hosts.push(format!(
        "good.example.com.{}.attacker.test",
        "h".repeat(5000 - 31)
    ));
    hosts
}

/// A refusal carries neither the host it refused nor that host as JSON writes it.
fn assert_not_echoed(body: &str, authority: &str) {
    let as_json = serde_json::to_string(authority).unwrap();
    assert!(!body.contains(authority), "{authority:?} is in {body:?}");
    assert!(
        !body.contains(as_json.trim_matches('"')),
        "{authority:?} is in {body:?}"
    );
    assert!(body.is_ascii(), "{body:?}");
    assert!(body.len() < 600, "{} bytes", body.len());
}

async fn authorize(
    srv: &Server,
    client_id: &str,
    redirect_uri: &str,
    scope: &str,
) -> reqwest::Response {
    http()
        .get(format!(
            "{}/authorize?{}",
            srv.base,
            form(&[
                ("response_type", "code"),
                ("client_id", client_id),
                ("redirect_uri", redirect_uri),
                ("code_challenge", CHALLENGE),
                ("code_challenge_method", "S256"),
                ("resource", &format!("{}/mcp", srv.base)),
                ("state", "from-the-app"),
                ("scope", scope),
            ])
        ))
        .send()
        .await
        .unwrap()
}

async fn register(srv: &Server, application_type: &str, uris: &[&str]) -> reqwest::Response {
    http()
        .post(format!("{}/oauth/register", srv.base))
        .json(&json!({
            "client_name": "ChatGPT",
            "redirect_uris": uris,
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
            "application_type": application_type,
        }))
        .send()
        .await
        .unwrap()
}

async fn token(srv: &Server, pairs: &[(&str, &str)]) -> reqwest::Response {
    http()
        .post(format!("{}/oauth/token", srv.base))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form(pairs))
        .send()
        .await
        .unwrap()
}

/// One MCP call with an access token: the agent the grant acts as, or the HTTP status of
/// a refusal.
async fn mcp_agent(srv: &Server, access: &str) -> Result<String, u16> {
    let probe = http()
        .post(format!("{}/mcp", srv.base))
        .header("authorization", format!("Bearer {access}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body("{}")
        .send()
        .await
        .unwrap();
    if probe.status() == 401 {
        return Err(401);
    }
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("{}/mcp", srv.base))
            .auth_header(access),
    );
    let client = ClientConfig::default().serve(transport).await.unwrap();
    let res = client
        .call_tool(
            CallToolRequestParams::new("whats_new".to_string())
                .with_arguments(json!({}).as_object().cloned().unwrap()),
        )
        .await
        .unwrap();
    assert_ne!(res.is_error, Some(true), "{res:?}");
    Ok(res.structured_content.unwrap()["agent"]
        .as_str()
        .unwrap()
        .to_string())
}

/// From an authorization request to an approved code: the wait page, the local approval,
/// and the redirect the browser would be sent to, which is returned and never followed.
async fn approve(srv: &Server, authorized: reqwest::Response) -> String {
    assert_eq!(authorized.status(), 303);
    let wait = authorized
        .headers()
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(wait.starts_with("/oauth/wait/"), "{wait}");
    let pending_url = format!(
        "{}{}",
        srv.base,
        wait.replacen("/oauth/wait/", "/oauth/pending/", 1)
    );
    let pending: Value = http()
        .get(&pending_url)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(pending["status"], "pending");
    srv.store
        .approve_oauth(Approval {
            user_code: pending["user_code"].as_str().unwrap().to_string(),
            agent: "chatgpt".into(),
            project_globs: vec![],
            label: None,
        })
        .unwrap();
    let decided: Value = http()
        .get(&pending_url)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    decided["redirect"].as_str().unwrap().to_string()
}

/// Every hostile host is refused at `/oauth/register` with the RFC 7591 error, the same
/// short description, no copy of what was sent, and no stored row.
#[tokio::test]
async fn registration_over_http_refuses_a_redirect_host_that_is_not_plain() {
    let hostile = hostile_authorities();
    // One server for each few requests: registration is rate limited for each address.
    for chunk in hostile.chunks(another::oauth_http::REGISTER_BURST as usize / 2) {
        let srv = start().await;
        for authority in chunk {
            // A loopback look-alike is tried as the native client that could use it.
            for (scheme, application_type) in [("https", "web"), ("http", "native")] {
                let uri = format!("{scheme}://{authority}/cb");
                let res = register(&srv, application_type, &[&uri]).await;
                assert_eq!(res.status(), 400, "{uri:?}");
                let body = res.text().await.unwrap();
                let parsed: Value = serde_json::from_str(&body).unwrap();
                assert_eq!(parsed["error"], "invalid_redirect_uri", "{uri:?}: {body}");
                assert_eq!(
                    parsed["error_description"],
                    format!("redirect_uris[0]: {REFUSED}"),
                    "{uri:?}"
                );
                assert_not_echoed(&body, authority);
            }
        }
        assert_eq!(srv.client_rows(), 0);
        assert!(srv.store.audit_log(100).unwrap().is_empty());
        srv.ct.cancel();
    }
}

/// The clients main serves register and complete a whole flow: register, authorize,
/// approve, token, refresh, an authenticated MCP call and revocation. A loopback client
/// asks for another port than the one it registered.
#[tokio::test]
async fn ordinary_clients_register_and_complete_a_flow() {
    for (application_type, registered, requested, shown) in [
        (
            "web",
            "https://chatgpt.com/connector_platform_oauth_redirect",
            "https://chatgpt.com/connector_platform_oauth_redirect",
            "chatgpt.com",
        ),
        (
            "web",
            "https://claude.ai/api/mcp/auth_callback",
            "https://claude.ai/api/mcp/auth_callback",
            "claude.ai",
        ),
        (
            "web",
            "https://app.example.com:8443/cb",
            "https://app.example.com:8443/cb",
            "app.example.com",
        ),
        (
            "web",
            "https://xn--bcher-kva.example/cb",
            "https://xn--bcher-kva.example/cb",
            "xn--bcher-kva.example",
        ),
        (
            "native",
            "http://localhost:8765/callback",
            "http://localhost:9999/callback",
            "localhost",
        ),
        (
            "native",
            "http://127.0.0.1:5173/cb",
            "http://127.0.0.1:6001/cb",
            "127.0.0.1",
        ),
        (
            "native",
            "http://[::1]:5173/cb",
            "http://[::1]:6001/cb",
            "[::1]",
        ),
    ] {
        let srv = start().await;
        let res = register(&srv, application_type, &[registered]).await;
        assert_eq!(res.status(), 201, "{registered}");
        let client: Value = res.json().await.unwrap();
        let client_id = client["client_id"].as_str().unwrap().to_string();

        let authorized = authorize(&srv, &client_id, requested, "another offline_access").await;
        assert_eq!(authorized.status(), 303, "{requested}");
        // The operator is shown the host as it is, with no warning.
        let listed = srv.cli(&["oauth", "pending"]);
        assert!(
            listed.contains(&format!("  \"ChatGPT\"  redirect host {shown}  expires ")),
            "{listed}"
        );
        assert!(!listed.contains("warning"), "{listed}");

        let back = approve(&srv, authorized).await;
        assert!(back.starts_with(requested), "{requested}");
        let code = query_param(&back, "code");
        let resource = format!("{}/mcp", srv.base);
        let issued = token(
            &srv,
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", requested),
                ("client_id", &client_id),
                ("code_verifier", VERIFIER),
                ("resource", &resource),
            ],
        )
        .await;
        assert_eq!(issued.status(), 200, "{requested}");
        let issued: Value = issued.json().await.unwrap();
        let refresh = issued["refresh_token"].as_str().unwrap().to_string();

        let rotated = token(
            &srv,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh),
                ("client_id", &client_id),
                ("resource", &resource),
            ],
        )
        .await;
        assert_eq!(rotated.status(), 200, "{requested}");
        let rotated: Value = rotated.json().await.unwrap();
        let access = rotated["access_token"].as_str().unwrap().to_string();
        assert_eq!(mcp_agent(&srv, &access).await.as_deref(), Ok("chatgpt"));

        let listed = srv.cli(&["oauth", "list"]);
        assert_eq!(listed.lines().count(), 1, "{listed}");
        assert!(!listed.contains("not plain"), "{listed}");

        let revoked = http()
            .post(format!("{}/oauth/revoke", srv.base))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(form(&[("token", &access)]))
            .send()
            .await
            .unwrap();
        assert_eq!(revoked.status(), 200);
        assert_eq!(mcp_agent(&srv, &access).await, Err(401), "{requested}");
        srv.ct.cancel();
    }
}

/// A client stored with a host that is not plain, written by SQL as a build before D100
/// stored it, cannot start an authorization request: 400 with no redirect, no request row,
/// nothing in `oauth pending`, one denied audit row, and a mark in `oauth list`.
#[tokio::test]
async fn a_stored_client_with_a_host_that_is_not_plain_is_refused_at_authorize() {
    let hostile = hostile_authorities();
    for chunk in hostile.chunks(another::oauth_http::AUTHORIZE_BURST as usize / 2) {
        let srv = start().await;
        let res = register(&srv, "web", &["https://chatgpt.com/cb"]).await;
        let client: Value = res.json().await.unwrap();
        let client_id = client["client_id"].as_str().unwrap().to_string();
        for authority in chunk {
            let uri = format!("https://{authority}/cb");
            // Asked for by itself, then beside a plain redirect URI the request names.
            for (stored, requested) in [
                (vec![uri.as_str()], uri.as_str()),
                (
                    vec!["https://chatgpt.com/cb", uri.as_str()],
                    "https://chatgpt.com/cb",
                ),
            ] {
                srv.store_redirects(&client_id, &stored);
                let res = authorize(&srv, &client_id, requested, "another").await;
                assert_eq!(res.status(), 400, "{uri:?}");
                assert!(res.headers().get("location").is_none(), "{uri:?}");
                let body = res.text().await.unwrap();
                assert!(body.contains("invalid_client"), "{body}");
                assert!(body.contains(CLIENT_REFUSED), "{body}");
                assert_not_echoed(&body, authority);
            }
            assert_eq!(srv.request_rows(), 0, "{uri:?}");
            assert!(srv.store.oauth_pending().unwrap().is_empty());
        }
        assert_eq!(
            srv.cli(&["oauth", "pending"]).trim(),
            "no clients are waiting. Connect the app, then approve the code it shows."
        );
        // The denied rows are written off the request's task.
        let mut denied = 0;
        for _ in 0..100 {
            let audit = serde_json::to_value(srv.store.audit_log(1000).unwrap()).unwrap();
            let rows: Vec<&Value> = audit
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["operation"] == "oauth.authorize")
                .collect();
            denied = rows.len();
            if denied == chunk.len() * 2 {
                for row in rows {
                    assert_eq!(row["outcome"], "denied");
                    let detail = row["detail"].as_str().unwrap();
                    assert!(
                        detail.starts_with(&format!("client \"{client_id}\" redirect host "))
                            && detail.ends_with(" is not plain")
                            && detail.is_ascii(),
                        "{detail:?}"
                    );
                }
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(denied, chunk.len() * 2);

        // The last stored value: a plain redirect URI and the chunk's last hostile one.
        let listed = srv.cli(&["oauth", "list"]);
        let rows: Vec<&str> = listed.lines().collect();
        assert_eq!(rows.len(), 2, "{listed}");
        assert_eq!(rows[0], "no OAuth grants");
        let escaped: String = chunk
            .last()
            .unwrap()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                    c.to_string()
                } else {
                    format!("\\u{{{:04X}}}", u32::from(c))
                }
            })
            .collect();
        assert_eq!(
            rows[1],
            format!(
                "client \"{client_id}\"  \"ChatGPT\"  redirect host {escaped}  is not a plain host name or IP address: it can no longer be approved and must register again"
            )
        );
        srv.ct.cancel();
    }
}

/// What happens to a grant issued before D100 to a client whose host is not plain: the
/// operator approved it, nothing more is sent to its redirect URI, and it keeps working
/// until the operator revokes it. `oauth list` marks it. It cannot be approved again.
#[tokio::test]
async fn a_grant_issued_before_the_rule_keeps_working_and_is_marked() {
    let srv = start().await;
    let plain = "https://chatgpt.com/cb";
    let hostile = "https://b\u{fc}cher.example/cb";
    let res = register(&srv, "web", &[plain]).await;
    let client: Value = res.json().await.unwrap();
    let client_id = client["client_id"].as_str().unwrap().to_string();
    let resource = format!("{}/mcp", srv.base);

    let authorized = authorize(&srv, &client_id, plain, "another offline_access").await;
    let back = approve(&srv, authorized).await;
    let issued = token(
        &srv,
        &[
            ("grant_type", "authorization_code"),
            ("code", &query_param(&back, "code")),
            ("redirect_uri", plain),
            ("client_id", &client_id),
            ("code_verifier", VERIFIER),
            ("resource", &resource),
        ],
    )
    .await;
    assert_eq!(issued.status(), 200);
    let issued: Value = issued.json().await.unwrap();
    let refresh = issued["refresh_token"].as_str().unwrap().to_string();

    // The row as an older build stored it.
    srv.store_redirects(&client_id, &[hostile]);

    let rotated = token(
        &srv,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh),
            ("client_id", &client_id),
            ("resource", &resource),
        ],
    )
    .await;
    assert_eq!(rotated.status(), 200);
    let rotated: Value = rotated.json().await.unwrap();
    let access = rotated["access_token"].as_str().unwrap().to_string();
    assert_eq!(mcp_agent(&srv, &access).await.as_deref(), Ok("chatgpt"));

    let listed = srv.cli(&["oauth", "list"]);
    let rows: Vec<&str> = listed.lines().collect();
    assert_eq!(rows.len(), 2, "{listed}");
    assert!(
        rows[0].contains("  active  \"ChatGPT\"  chatgpt  projects none")
            && rows[0].ends_with("  [the redirect host of its client is not plain]"),
        "{listed}"
    );
    assert_eq!(
        rows[1],
        format!(
            "client \"{client_id}\"  \"ChatGPT\"  redirect host b\\u{{00FC}}cher.example  is not a plain host name or IP address: it can no longer be approved and must register again"
        )
    );
    let grants: Value = serde_json::from_str(&srv.cli(&["--json", "oauth", "list"])).unwrap();
    assert_eq!(
        grants[0]["client_redirect_not_plain"],
        json!(["b\\u{00FC}cher.example"])
    );
    let grant_id = grants[0]["grant_id"].as_str().unwrap().to_string();

    // It cannot be approved again.
    let again = authorize(&srv, &client_id, hostile, "another").await;
    assert_eq!(again.status(), 400);
    assert!(again.headers().get("location").is_none());
    assert!(srv.store.oauth_pending().unwrap().is_empty());

    // Revoking is the operator's, and ends it.
    srv.cli(&["oauth", "revoke", &grant_id]);
    assert_eq!(mcp_agent(&srv, &access).await, Err(401));
    // Nothing was deleted or rewritten: the stored client row is as it was.
    let stored: String = srv
        .db()
        .query_row(
            "SELECT redirect_uris FROM oauth_clients WHERE client_id = ?1",
            [&client_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, serde_json::to_string(&[hostile]).unwrap());
    srv.ct.cancel();
}

/// Serve one client metadata document on loopback, once, and return its URL.
async fn serve_document(redirect_uris: &[&str]) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client_id = format!("http://{}/client.json", listener.local_addr().unwrap());
    let doc = json!({
        "client_id": client_id,
        "client_name": "Claude",
        "redirect_uris": redirect_uris,
        "token_endpoint_auth_method": "none"
    })
    .to_string();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 2048];
        let _ = sock.read(&mut buf).await;
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            doc.len()
        );
        let _ = sock.write_all(head.as_bytes()).await;
        let _ = sock.write_all(doc.as_bytes()).await;
    });
    client_id
}

/// A client metadata document is held to the same rule: one hostile redirect URI in it
/// and the client cannot start a request, even for a plain URI beside it.
#[tokio::test]
async fn a_client_metadata_document_is_held_to_the_same_rule() {
    let hostile = hostile_authorities();
    for chunk in hostile.chunks(another::oauth_http::AUTHORIZE_BURST as usize - 2) {
        let srv = start().await;
        for authority in chunk {
            let uri = format!("https://{authority}/cb");
            let plain = "https://claude.ai/api/mcp/auth_callback";
            let client_id = serve_document(&[plain, &uri]).await;
            let res = authorize(&srv, &client_id, plain, "another").await;
            assert_eq!(res.status(), 400, "{uri:?}");
            assert!(res.headers().get("location").is_none(), "{uri:?}");
            let body = res.text().await.unwrap();
            assert!(body.contains("invalid_client"), "{body}");
            assert!(
                body.contains("redirect_uris[1]: redirect URI is not allowed")
                    && body.contains("a plain host name or IP address"),
                "{uri:?}: {body}"
            );
            assert_not_echoed(&body, authority);
            assert_eq!(srv.request_rows(), 0, "{uri:?}");
        }
        // The documents main serves are still accepted.
        let client_id = serve_document(&[
            "https://claude.ai/api/mcp/auth_callback",
            "http://127.0.0.1/callback",
            "http://localhost/callback",
        ])
        .await;
        let res = authorize(
            &srv,
            &client_id,
            "http://127.0.0.1:3118/callback",
            "another",
        )
        .await;
        assert_eq!(res.status(), 303);
        assert_eq!(srv.request_rows(), 1);
        srv.ct.cancel();
    }
}
