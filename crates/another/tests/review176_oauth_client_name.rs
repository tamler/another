//! Adversarial review of #176: `another serve` prints a registering client's name to the
//! operator's terminal (`oauth_http.rs`, "{client_name} wants to connect"), outside `emit`.
//! Dynamic client registration is unauthenticated, so the name must not carry control
//! characters into that line.

use std::net::SocketAddr;

use another::oauth_http::parse_public_url;
use another::serve::{http_router, HttpOptions};
use another_core::Store;
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[tokio::test]
async fn a_registered_client_name_cannot_carry_terminal_control_characters() {
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
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { stop.cancelled_owned().await })
        .await
        .unwrap();
    });

    let resp = reqwest::Client::new()
        .post(format!("{base}/oauth/register"))
        .json(&json!({
            "client_name": "Helper\u{1b}]0;pwned\u{7}\u{1b}[2J",
            "redirect_uris": ["https://example.com/cb"],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
            "application_type": "web"
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap();
    // If it registered, the authorization request is what makes `another serve` print the
    // name ("... wants to connect") on the operator's terminal.
    if let Some(client_id) = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v["client_id"].as_str().map(str::to_string))
    {
        let url = format!(
            "{base}/authorize?response_type=code&client_id={client_id}\
             &redirect_uri=https%3A%2F%2Fexample.com%2Fcb&state=s\
             &code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM\
             &code_challenge_method=S256&resource={}",
            urlencode(&format!("{base}/mcp"))
        );
        let r = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
            .get(url)
            .send()
            .await
            .unwrap();
        println!("authorize status: {}", r.status());
    }
    ct.cancel();
    server.await.unwrap();

    println!("register status: {status}; body: {body}");
    assert!(
        status.is_client_error(),
        "a client_name with ESC/BEL was accepted ({status}); `another serve` later prints it raw: {body}"
    );
}
