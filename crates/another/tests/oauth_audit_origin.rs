//! OAuth audit rows record where the request came from by the same rule as MCP calls: the
//! socket address, and a proxy header only as a labelled claim next to it
//! (FINDINGS-main-oauth-audit-origin; the rows used to keep only the claim, or nothing). A
//! registration is used here: it writes one `oauth.register` audit row and needs no approval.

use std::net::SocketAddr;

use another::oauth_http::parse_public_url;
use another::serve::{http_router, HttpOptions};
use another_core::Store;
use serde_json::json;
use tokio_util::sync::CancellationToken;

async fn start() -> (tempfile::TempDir, Store, String, CancellationToken) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let ct = CancellationToken::new();
    let opts = HttpOptions {
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
    (dir, store, base, ct)
}

async fn register(store: &Store, base: &str, header: Option<(&str, &str)>) -> Option<String> {
    let before = store.audit_log(1000).unwrap().len();
    let mut req = reqwest::Client::new()
        .post(format!("{base}/oauth/register"))
        .json(&json!({
            "client_name": "probe",
            "redirect_uris": ["http://127.0.0.1:3118/callback"],
            "application_type": "native",
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
        }));
    if let Some((k, v)) = header {
        req = req.header(k, v);
    }
    let status = req.send().await.unwrap().status();
    assert!(status.is_success(), "{status}");
    let log = store.audit_log(1000).unwrap();
    assert_eq!(log.len(), before + 1, "one audit row per registration");
    log.last().unwrap().remote.clone()
}

#[tokio::test]
async fn an_oauth_audit_row_keeps_the_socket_address() {
    let (_dir, store, base, ct) = start().await;
    let bare = register(&store, &base, None).await;
    let claimed = register(&store, &base, Some(("x-forwarded-for", "203.0.113.9"))).await;
    ct.cancel();
    assert!(
        bare.as_deref().is_some_and(|r| r.starts_with("127.0.0.1:")),
        "no proxy header: the row should record the socket address, got {bare:?}"
    );
    assert!(
        claimed
            .as_deref()
            .is_some_and(|r| r.starts_with("x-forwarded-for: 203.0.113.9 via 127.0.0.1:")),
        "a proxy claim should be recorded next to the socket address, got {claimed:?}"
    );
}

fn form(pairs: &[(&str, &str)]) -> String {
    let enc = |s: &str| -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn param(url: &str, key: &str) -> String {
    url.split(&format!("{key}="))
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap()
        .to_string()
}

/// The audit row `operation` last wrote.
fn origin_of(store: &Store, operation: &str) -> Option<String> {
    store
        .audit_log(1000)
        .unwrap()
        .into_iter()
        .rev()
        .find(|e| e.operation == operation)
        .unwrap_or_else(|| panic!("no {operation} audit row"))
        .remote
}

/// Every OAuth audit source, not only registration, records the socket: an authorization,
/// a code exchange and a refresh without a proxy header, and a revocation with one.
#[tokio::test]
async fn every_oauth_audit_source_records_the_socket() {
    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    let (dir, store, base, ct) = start().await;
    let work = dir.path().join("work").join("shop");
    store.ensure_project("t", &work.to_string_lossy()).unwrap();
    store
        .register_agent(another_core::RegisterAgent {
            name: "probe".into(),
            program: another_core::Program::Other,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let redirect = "http://127.0.0.1:3118/callback";
    let resource = format!("{base}/mcp");
    let registered: serde_json::Value = http
        .post(format!("{base}/oauth/register"))
        .json(&json!({
            "client_name": "probe",
            "redirect_uris": [redirect],
            "application_type": "native",
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let client_id = registered["client_id"].as_str().unwrap().to_string();

    let query = form(&[
        ("response_type", "code"),
        ("client_id", client_id.as_str()),
        ("redirect_uri", redirect),
        ("code_challenge", CHALLENGE),
        ("code_challenge_method", "S256"),
        ("resource", resource.as_str()),
        ("scope", "another offline_access"),
    ]);
    let auth = http
        .get(format!("{base}/authorize?{query}"))
        .send()
        .await
        .unwrap();
    assert_eq!(auth.status(), 303);
    let wait = auth.headers()["location"].to_str().unwrap().to_string();
    let pending: serde_json::Value = http
        .get(format!(
            "{base}{}",
            wait.replacen("/oauth/wait/", "/oauth/pending/", 1)
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    store
        .approve_oauth(another_core::Approval {
            user_code: pending["user_code"].as_str().unwrap().into(),
            agent: "probe".into(),
            project_globs: vec![dir
                .path()
                .join("work")
                .join("*")
                .to_string_lossy()
                .replace('\\', "/")],
            label: None,
        })
        .unwrap();
    let decided: serde_json::Value = http
        .get(format!(
            "{base}{}",
            wait.replacen("/oauth/wait/", "/oauth/pending/", 1)
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let code = param(decided["redirect"].as_str().unwrap(), "code");

    let token = |body: String| {
        let http = http.clone();
        let base = base.clone();
        async move {
            http.post(format!("{base}/oauth/token"))
                .header("content-type", "application/x-www-form-urlencoded")
                .body(body)
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()
        }
    };
    let issued = token(form(&[
        ("grant_type", "authorization_code"),
        ("code", &code),
        ("redirect_uri", redirect),
        ("client_id", &client_id),
        ("code_verifier", VERIFIER),
        ("resource", &resource),
    ]))
    .await;
    let refresh = issued["refresh_token"].as_str().unwrap().to_string();
    let rotated = token(form(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", &refresh),
        ("client_id", &client_id),
        ("resource", &resource),
    ]))
    .await;
    let access = rotated["access_token"].as_str().unwrap().to_string();
    let revoked = http
        .post(format!("{base}/oauth/revoke"))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("x-forwarded-for", "203.0.113.9")
        .body(form(&[("token", &access)]))
        .send()
        .await
        .unwrap();
    assert!(revoked.status().is_success(), "{}", revoked.status());
    ct.cancel();

    for op in ["oauth.authorize", "oauth.token", "oauth.refresh"] {
        let got = origin_of(&store, op);
        assert!(
            got.as_deref().is_some_and(|r| r.starts_with("127.0.0.1:")),
            "{op}: the row should record the socket address, got {got:?}"
        );
    }
    let got = origin_of(&store, "oauth.revoke");
    assert!(
        got.as_deref()
            .is_some_and(|r| r.starts_with("x-forwarded-for: 203.0.113.9 via 127.0.0.1:")),
        "oauth.revoke: the claim should sit next to the socket, got {got:?}"
    );
}
