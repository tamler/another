//! The audit log records where a remote call came from. A proxy header is believed only as a
//! labelled claim: the stored origin names the header it came from ("x-forwarded-for:
//! 203.0.113.9"), so an operator reading `another audit` can tell a client-supplied address
//! from the socket address, which is stored bare. Only the first hop of a list is kept, and
//! `cf-connecting-ip` (set by the Cloudflare tunnel) wins over the others.

use std::net::SocketAddr;

use another::serve::{http_router, HttpOptions};
use another_core::Store;
use tokio_util::sync::CancellationToken;

struct Server {
    _dir: tempfile::TempDir,
    store: Store,
    base: String,
    _ct: CancellationToken,
}

async fn start() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let ct = CancellationToken::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let opts = HttpOptions {
        auth: true,
        bound_port: addr.port(),
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
        _dir: dir,
        store,
        base: format!("http://{addr}"),
        _ct: ct,
    }
}

/// A refused call (bad token) with these headers, and the origin its audit row records.
async fn origin(srv: &Server, headers: &[(&str, &str)]) -> Option<String> {
    let before = srv.store.audit_log(1000).unwrap().len();
    let mut req = reqwest::Client::new()
        .post(format!("{}/mcp", srv.base))
        .header("authorization", "Bearer ano_nope_nope")
        .header("content-type", "application/json")
        .body("{}");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    assert_eq!(req.send().await.unwrap().status(), 401);
    let log = srv.store.audit_log(1000).unwrap();
    assert_eq!(log.len(), before + 1, "one audit row per refused call");
    // `audit_log` lists the newest rows, oldest first.
    log.last().unwrap().remote.clone()
}

#[tokio::test]
async fn a_proxy_header_is_recorded_as_a_labelled_claim() {
    let srv = start().await;

    // No proxy header: the socket address, bare.
    let bare = origin(&srv, &[]).await.unwrap();
    assert!(bare.starts_with("127.0.0.1:"), "{bare}");
    assert!(
        !bare.contains(": "),
        "a socket address carries no header label: {bare}"
    );

    for (headers, want) in [
        (
            vec![("x-forwarded-for", "203.0.113.9, 10.0.0.1")],
            "x-forwarded-for: 203.0.113.9",
        ),
        (
            vec![("x-real-ip", " 198.51.100.4 ")],
            "x-real-ip: 198.51.100.4",
        ),
        (
            vec![
                ("x-forwarded-for", "203.0.113.9"),
                ("cf-connecting-ip", "192.0.2.7"),
            ],
            "cf-connecting-ip: 192.0.2.7",
        ),
        (
            vec![
                ("x-real-ip", "198.51.100.4"),
                ("x-forwarded-for", "203.0.113.9"),
            ],
            "x-forwarded-for: 203.0.113.9",
        ),
    ] {
        assert_eq!(
            origin(&srv, &headers).await.as_deref(),
            Some(want),
            "{headers:?}"
        );
    }
}
