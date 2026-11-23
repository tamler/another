use another::dashboard::DashboardMode;
use another::serve::{http_router, HttpOptions};
use another_core::Store;
use tokio_util::sync::CancellationToken;

async fn server(auth: bool) -> (tempfile::TempDir, Store, String, CancellationToken) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let ct = CancellationToken::new();
    let opts = HttpOptions {
        federation: true,
        auth,
        bound_port: addr.port(),
        dashboard: if auth {
            DashboardMode::Redacted
        } else {
            DashboardMode::Local
        },
        dashboard_bound: Some(addr),
        ..Default::default()
    };
    let app = http_router(store.clone(), ct.clone(), &opts);
    let stop = ct.clone();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(stop.cancelled_owned())
            .await
            .unwrap();
    });
    (dir, store, format!("http://{addr}"), ct)
}

#[tokio::test]
async fn federation_only_has_no_local_routes_even_with_rewritten_host() {
    let (_dir, _store, base, ct) = server(false).await;
    let client = reqwest::Client::new();
    for path in ["/mcp", "/activity", "/ui", "/api/dash", "/api/dash/events"] {
        for method in [reqwest::Method::GET, reqwest::Method::POST] {
            let response = client
                .request(method, format!("{base}{path}"))
                .body("{}")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 404, "{path}");
        }
    }
    let health: serde_json::Value = client
        .get(format!("{base}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["ok"], true);
    assert_eq!(health["dashboard"], "off");
    assert_eq!(
        client
            .post(format!("{base}/fed/pair"))
            .body("{}")
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert_eq!(
        client
            .get(format!("{base}/fed/pull"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    ct.cancel();
}

#[tokio::test]
async fn authenticated_federation_keeps_local_routes_token_gated() {
    let (_dir, store, base, ct) = server(true).await;
    store
        .register_agent(another_core::RegisterAgent {
            name: "reader".into(),
            program: another_core::Program::Codex,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    let token = store.create_token("reader", &[], None, None).unwrap();
    let client = reqwest::Client::new();
    for path in ["/mcp", "/activity", "/ui", "/api/dash"] {
        assert_eq!(
            client
                .get(format!("{base}{path}"))
                .send()
                .await
                .unwrap()
                .status(),
            401,
            "{path}"
        );
    }
    for path in ["/activity", "/ui", "/api/dash"] {
        assert_eq!(
            client
                .get(format!("{base}{path}"))
                .bearer_auth(&token.secret)
                .send()
                .await
                .unwrap()
                .status(),
            200,
            "{path}"
        );
    }
    ct.cancel();
}

#[tokio::test]
async fn unauthenticated_failure_burst_has_a_bounded_audit_log() {
    let (_dir, store, base, ct) = server(true).await;
    let client = reqwest::Client::new();
    for _ in 0..40 {
        assert_eq!(
            client
                .post(format!("{base}/fed/pair"))
                .body("{}")
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        assert_eq!(
            client
                .get(format!("{base}/fed/pull"))
                .header("x-another-node", "01ARZ3NDEKTSV4RRFFQ69G5FAV")
                .header("x-another-time", "0")
                .header("x-another-signature", "bad")
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        for path in ["/mcp", "/activity"] {
            assert_eq!(
                client
                    .get(format!("{base}{path}"))
                    .header("authorization", "Bearer ano_unknown_bad")
                    .send()
                    .await
                    .unwrap()
                    .status(),
                401
            );
        }
    }
    assert!(store.audit_log(1000).unwrap().len() <= 32);
    ct.cancel();
}
