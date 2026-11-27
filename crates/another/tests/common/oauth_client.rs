//! Shared OAuth HTTP client, form encoding, PKCE, and CIMD helpers for `crates/another/tests/`.
#![allow(dead_code)]

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const PKCE_VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
pub const PKCE_CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

pub fn no_redirect_http() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

pub fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

pub fn with_query(url: &str, pairs: &[(&str, &str)]) -> String {
    format!("{url}?{}", form(pairs))
}

pub fn query_param(url: &str, key: &str) -> String {
    url.split(&format!("{key}="))
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap()
        .to_string()
}

/// Serve one Client ID Metadata Document on loopback and return its `client_id` URL.
pub async fn serve_cimd_document(client_name: &str, redirect_uris: &[&str]) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client_id = format!("http://{}/client.json", listener.local_addr().unwrap());
    let doc = json!({
        "client_id": client_id,
        "client_name": client_name,
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
