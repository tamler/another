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
