//! Hardening regressions from docs/security-review-v0.1.0.md. Temporary ANOTHER_HOME,
//! ephemeral ports.

use std::net::SocketAddr;

use another::remote::ForwardedHeader;
use another::serve::{http_router, HttpOptions};
use another_core::{DecisionOption, NewDecision, Program, RegisterAgent, Store};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// An unauthenticated server (the default `another serve`, and the federation hub as
/// FEDERATION.md sets it up) answers as this machine's person. A web page that rebinds its
/// own name to 127.0.0.1, or a tunnel that forwards its public hostname, must not reach the
/// local API; loopback names still do, and the signature-authenticated /fed routes are not
/// affected.
#[tokio::test(flavor = "multi_thread")]
async fn unauthenticated_servers_only_answer_loopback_host_names() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.ensure_project("t", "/work/shop").unwrap();
    store
        .register_agent_trusted(RegisterAgent {
            name: "claude".into(),
            program: Program::Claude,
            project: Some("/work/shop".into()),
            model: None,
            description: None,
        })
        .unwrap();
    let asked = store
        .ask_decision(
            NewDecision {
                agent: "claude@shop".into(),
                question: "Approve the GPU lease?".into(),
                options: ["yes", "no"]
                    .map(|v| DecisionOption {
                        value: v.into(),
                        label: v.into(),
                        description: None,
                    })
                    .to_vec(),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    let id = asked.decision_id.clone();

    let ct = CancellationToken::new();
    let app = http_router(
        store.clone(),
        ct.clone(),
        &HttpOptions {
            federation: true,
            ..Default::default()
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
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
    let base = format!("http://{addr}");
    let http = reqwest::Client::new();
    let port = addr.port();

    // Foreign Host names: a rebinding page, a tunnel's public name, a lookalike.
    for host in [
        format!("evil.example:{port}"),
        "studio.my-tailnet.ts.net".to_string(),
        format!("127.0.0.1.evil.example:{port}"),
    ] {
        for path in ["/summary", "/activity", "/decisions", "/spend", "/lanes"] {
            let r = http
                .get(format!("{base}{path}"))
                .header(reqwest::header::HOST, &host)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 403, "GET {path} with Host {host}");
        }
        let r = http
            .post(format!("{base}/decisions/{id}/answer"))
            .header(reqwest::header::HOST, &host)
            .json(&json!({ "value": "yes" }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "answer with Host {host}");
        // Federation routes keep their own (signature) check and stay reachable.
        let r = http
            .get(format!("{base}/fed/pull"))
            .header(reqwest::header::HOST, &host)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 401, "/fed/pull with Host {host}");
    }
    let still_open = store.decision(&id, None).unwrap();
    assert_eq!(still_open.status.as_str(), "open");

    // Loopback names work as before.
    for host in [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ] {
        let r = http
            .get(format!("{base}/summary"))
            .header(reqwest::header::HOST, &host)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "Host {host}");
    }
    let r = http
        .post(format!("{base}/decisions/{id}/answer"))
        .json(&json!({ "value": "yes" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let answered: Value = r.json().await.unwrap();
    assert_eq!(answered["status"], "answered", "{answered}");
    ct.cancel();
}

async fn auth_server(opts: HttpOptions) -> (tempfile::TempDir, Store, String, CancellationToken) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let ct = CancellationToken::new();
    let app = http_router(store.clone(), ct.clone(), &opts);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
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
    (dir, store, format!("http://{addr}"), ct)
}

/// Twelve bad guesses, each from a different claimed address. Returns the status codes.
async fn guess_with_rotating_header(base: &str, header: &str) -> Vec<u16> {
    let http = reqwest::Client::new();
    let mut statuses = Vec::new();
    for i in 0..12 {
        let r = http
            .post(format!("{base}/join"))
            .header(header, format!("203.0.113.{i}"))
            .json(&json!({ "code": "anj_guess" }))
            .send()
            .await
            .unwrap();
        statuses.push(r.status().as_u16());
    }
    statuses
}

/// Finding 6: without an explicit `--trusted-proxy-header`, a client can neither rotate past
/// the `/join` limit with forwarding headers nor choose the address the audit log records.
#[tokio::test(flavor = "multi_thread")]
async fn forwarding_headers_are_ignored_by_default() {
    for header in ["x-forwarded-for", "cf-connecting-ip", "x-real-ip"] {
        let (_dir, store, base, ct) = auth_server(HttpOptions {
            auth: true,
            ..Default::default()
        })
        .await;
        let statuses = guess_with_rotating_header(&base, header).await;
        assert_eq!(*statuses.last().unwrap(), 429, "{header}: {statuses:?}");
        for entry in store.audit_log(50).unwrap() {
            let remote = entry.remote.unwrap_or_default();
            assert!(!remote.contains("203.0.113"), "{header}: {remote}");
            assert!(remote.starts_with("127.0.0.1:"), "{header}: {remote}");
        }
        ct.cancel();
    }
}

/// With `--trusted-proxy-header`, each client behind the tunnel has its own limit and its real
/// address in the audit log; only the last value counts, the one the tunnel added.
#[tokio::test(flavor = "multi_thread")]
async fn a_trusted_proxy_header_names_the_client_by_its_last_value() {
    let (_dir, store, base, ct) = auth_server(HttpOptions {
        auth: true,
        trusted_proxy_header: Some(ForwardedHeader::XForwardedFor),
        ..Default::default()
    })
    .await;
    let statuses = guess_with_rotating_header(&base, "x-forwarded-for").await;
    assert!(!statuses.contains(&429), "{statuses:?}");
    assert!(store
        .audit_log(50)
        .unwrap()
        .iter()
        .any(|e| e.remote.as_deref() == Some("x-forwarded-for: 203.0.113.11")));

    // One client spoofing the first hop stays one client.
    let http = reqwest::Client::new();
    let mut statuses = Vec::new();
    for i in 0..12 {
        let r = http
            .post(format!("{base}/join"))
            .header("x-forwarded-for", format!("198.51.100.{i}, 2001:db8::7"))
            .json(&json!({ "code": "anj_guess" }))
            .send()
            .await
            .unwrap();
        statuses.push(r.status().as_u16());
    }
    assert_eq!(*statuses.last().unwrap(), 429, "{statuses:?}");

    // Headers other than the named one are still ignored.
    let (_dir2, _store2, base2, ct2) = auth_server(HttpOptions {
        auth: true,
        trusted_proxy_header: Some(ForwardedHeader::CfConnectingIp),
        ..Default::default()
    })
    .await;
    let statuses = guess_with_rotating_header(&base2, "x-forwarded-for").await;
    assert_eq!(*statuses.last().unwrap(), 429, "{statuses:?}");
    ct.cancel();
    ct2.cancel();
}

#[test]
fn the_trusted_proxy_header_flag_needs_auth() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["serve", "--trusted-proxy-header", "x-forwarded-for"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--auth"));
}
