//! Hardening regressions from docs/security-review-v0.1.0.md. Temporary ANOTHER_HOME,
//! ephemeral ports.

use std::net::SocketAddr;

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
