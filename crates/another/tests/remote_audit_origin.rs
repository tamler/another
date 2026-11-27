//! The audit log records where a remote call came from. A proxy header is only a labelled
//! claim, recorded next to the socket address and never instead of it ("x-forwarded-for:
//! 203.0.113.9 via 127.0.0.1:54321"), so an operator reading `another audit` can tell a
//! client-supplied address from where the call really connected. Only the first hop of a
//! list is kept, cut to 64 characters; an empty claim counts as no header; and
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
        legacy_repair: true,
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
        let got = origin(&srv, &headers).await.unwrap();
        let (claim, socket) = got
            .split_once(" via ")
            .unwrap_or_else(|| panic!("{headers:?}: no socket address in {got:?}"));
        assert_eq!(claim, want, "{headers:?}");
        assert!(socket.starts_with("127.0.0.1:"), "{headers:?}: {got:?}");
    }

    // A claim is cut to 64 characters, so a client cannot bloat every audit row.
    let long = "9".repeat(4000);
    let got = origin(&srv, &[("x-forwarded-for", long.as_str())])
        .await
        .unwrap();
    let claim = got.split_once(" via ").unwrap().0;
    assert_eq!(claim, format!("x-forwarded-for: {}", "9".repeat(64)));
    // An empty claim is no claim: the socket address alone.
    let empty = origin(&srv, &[("x-forwarded-for", "")]).await.unwrap();
    assert!(
        empty.starts_with("127.0.0.1:") && !empty.contains(": "),
        "{empty}"
    );
}

/// adv231 (FINDINGS-231-w9W, applied as-is apart from this comment): a proxy header is a
/// client claim, so it is recorded next to the socket address, not instead of it. Before the
/// fix, a direct caller that set `x-forwarded-for` (even to nothing) left no trace of where
/// it actually connected from.
#[tokio::test]
async fn adv231_gap_a_proxy_claim_keeps_the_socket_address() {
    let srv = start().await;
    let mut kept = Vec::new();
    for value in ["203.0.113.9", ""] {
        let got = origin(&srv, &[("x-forwarded-for", value)]).await.unwrap();
        if !got.contains("127.0.0.1:") {
            kept.push(got);
        }
    }
    assert!(
        kept.is_empty(),
        "the socket address is replaced by the client's claim: {kept:?}"
    );
}
