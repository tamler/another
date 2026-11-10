//! Dashboard HTTP: loopback UI, redacted auth mode, and the confirm gate.

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::{Command, Stdio};

use another::dashboard::{resolve_dashboard, DashboardMode};
use another::serve::{http_router, HttpOptions};
use another_core::{AcquireHolds, NewRequest, Program, RegisterAgent, SendMessage, Store};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

struct Running {
    base: String,
    store: Store,
    _dir: tempfile::TempDir,
    ct: CancellationToken,
}

async fn serve(opts: HttpOptions) -> Running {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .register_agent(RegisterAgent {
            name: "alice".into(),
            program: Program::Claude,
            project: Some("/work/shop".into()),
            model: None,
            description: None,
        })
        .unwrap();
    store
        .register_agent(RegisterAgent {
            name: "bob".into(),
            program: Program::Codex,
            project: Some("/work/shop".into()),
            model: None,
            description: None,
        })
        .unwrap();
    store
        .send_message(SendMessage {
            from: "alice".into(),
            to: vec!["bob".into()],
            subject: "Cutover".into(),
            body: "SECRET BODY for the local dashboard only".into(),
            request: Some(NewRequest {
                due_in_seconds: Some(3600),
            }),
            ..Default::default()
        })
        .unwrap();
    store
        .acquire_holds(AcquireHolds {
            agent: "alice".into(),
            project: "shop".into(),
            paths: vec!["src/**".into()],
            ttl_seconds: Some(3600),
            exclusive: true,
            reason: Some("editing".into()),
            repo_wide: false,
        })
        .unwrap();
    let ct = CancellationToken::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut opts = opts;
    opts.bound_port = addr.port();
    opts.dashboard_bound = Some(addr);
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
    Running {
        base: format!("http://{addr}"),
        store,
        _dir: dir,
        ct,
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

async fn raw_status(authority: &str, request: &str) -> u16 {
    let authority = authority.to_string();
    let request = request.to_string();
    tokio::task::spawn_blocking(move || raw_status_blocking(&authority, &request))
        .await
        .unwrap()
}

fn raw_status_blocking(authority: &str, request: &str) -> u16 {
    use std::io::{Read, Write};
    let mut sock = std::net::TcpStream::connect(authority).unwrap();
    sock.set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    sock.set_write_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    sock.write_all(request.as_bytes()).unwrap();
    let _ = sock.flush();
    let mut buf = Vec::new();
    let read = sock.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let status = text
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    assert!(
        status != 0,
        "no status from {authority}: read={read:?} body={text:?} req={request:?}"
    );
    status
}

#[tokio::test]
async fn foreign_host_is_refused_on_every_dashboard_route() {
    let srv = serve(HttpOptions {
        dashboard: DashboardMode::Local,
        ..Default::default()
    })
    .await;
    let authority = srv.base.trim_start_matches("http://");
    let gets = [
        "/ui",
        "/ui/",
        "/ui/app.css",
        "/ui/app.js",
        "/api/dash",
        "/api/dash/activity",
        "/api/dash/revision",
        "/api/dash/stream",
    ];
    for path in gets {
        let status = raw_status(
            authority,
            &format!("GET {path} HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert_eq!(status, 403, "{path}");
    }
    let posts = [
        "/api/dash/holds/nope/release",
        "/api/dash/requests/nope/expire",
    ];
    for path in posts {
        let status = raw_status(
            authority,
            &format!(
                "POST {path} HTTP/1.1\r\nHost: evil.example\r\nContent-Type: application/json\r\nContent-Length: 16\r\nX-Another-Confirm: 1\r\nConnection: close\r\n\r\n{{\"confirm\":true}}"
            ),
        )
        .await;
        assert_eq!(status, 403, "{path} with a foreign Host");
        let status = raw_status(
            authority,
            &format!(
                "POST {path} HTTP/1.1\r\nHost: evil.example\r\nOrigin: http://evil.example\r\nContent-Type: application/json\r\nContent-Length: 16\r\nX-Another-Confirm: 1\r\nConnection: close\r\n\r\n{{\"confirm\":true}}"
            ),
        )
        .await;
        assert_eq!(
            status, 403,
            "{path} with Host evil.example and Origin http://evil.example"
        );
    }
    // A loopback Host with an Origin that only looks similar must still fail.
    let disguised = raw_status(
        authority,
        &format!(
            "POST /api/dash/requests/nope/expire HTTP/1.1\r\nHost: {authority}\r\nOrigin: http://127.0.0.1.evil.example\r\nContent-Type: application/json\r\nContent-Length: 16\r\nX-Another-Confirm: 1\r\nConnection: close\r\n\r\n{{\"confirm\":true}}"
        ),
    )
    .await;
    assert_eq!(disguised, 403);
    srv.ct.cancel();
}

#[tokio::test]
async fn local_dashboard_shows_bodies_and_requires_confirm() {
    let srv = serve(HttpOptions {
        dashboard: DashboardMode::Local,
        ..Default::default()
    })
    .await;
    let http = client();

    let ui = http.get(format!("{}/ui", srv.base)).send().await.unwrap();
    assert_eq!(ui.status(), 200);
    let html = ui.text().await.unwrap();
    assert!(html.contains("From one agent to another"));
    assert!(html.contains("/ui/app.js"));
    assert!(!html.to_lowercase().contains("cdn"));
    assert!(!html.contains("googleapis"));
    let css = http
        .get(format!("{}/ui/app.css", srv.base))
        .send()
        .await
        .unwrap();
    assert_eq!(css.status(), 200);
    assert!(!css.text().await.unwrap().contains("@import"));
    let js = http
        .get(format!("{}/ui/app.js", srv.base))
        .send()
        .await
        .unwrap();
    assert_eq!(js.status(), 200);

    let health: Value = serde_json::from_str(
        &http
            .get(format!("{}/health", srv.base))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(health["dashboard"], "local");
    assert_eq!(health["auth"], false);

    let snap_res = http
        .get(format!("{}/api/dash", srv.base))
        .send()
        .await
        .unwrap();
    assert_eq!(
        snap_res
            .headers()
            .get("x-content-type-options")
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );
    let snap: Value = serde_json::from_str(&snap_res.text().await.unwrap()).unwrap();
    let blob = snap.to_string();
    assert!(
        blob.contains("SECRET BODY for the local dashboard only"),
        "{blob}"
    );
    assert_eq!(snap["access"]["actions"], true);
    let request_id = snap["requests"][0]["request_id"].as_str().unwrap();
    let hold_id = snap["holds"][0]["hold_id"].as_str().unwrap();

    let activity = http
        .get(format!("{}/activity", srv.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        !activity.contains("SECRET BODY"),
        "the shared activity feed stays body-free"
    );

    let found: Value = serde_json::from_str(
        &http
            .get(format!(
                "{}/api/dash/activity?q=SECRET%20BODY&since=1d",
                srv.base
            ))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(found["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["body"].as_str().unwrap_or("").contains("SECRET BODY")));

    let denied = http
        .post(format!(
            "{}/api/dash/requests/{request_id}/expire",
            srv.base
        ))
        .header("content-type", "application/json")
        .body(r#"{"confirm":true}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 400);

    let cross = http
        .post(format!(
            "{}/api/dash/requests/{request_id}/expire",
            srv.base
        ))
        .header("content-type", "application/json")
        .header("x-another-confirm", "1")
        .header("origin", "http://evil.example")
        .body(r#"{"confirm":true}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(cross.status(), 403);

    let expired: Value = serde_json::from_str(
        &http
            .post(format!(
                "{}/api/dash/requests/{request_id}/expire",
                srv.base
            ))
            .header("content-type", "application/json")
            .header("x-another-confirm", "1")
            .header("origin", &srv.base)
            .body(r#"{"confirm":true}"#)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(expired["status"], "stale");

    let fresh = http
        .post(format!("{}/api/dash/holds/{hold_id}/release", srv.base))
        .header("content-type", "application/json")
        .header("x-another-confirm", "1")
        .header("origin", &srv.base)
        .body(r#"{"confirm":true}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(
        fresh.status(),
        409,
        "a live unconflicted hold is not released"
    );

    let mut stream = http
        .get(format!("{}/api/dash/stream", srv.base))
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), 200);
    let chunk = tokio::time::timeout(std::time::Duration::from_secs(3), stream.chunk())
        .await
        .expect("sse timed out")
        .unwrap()
        .unwrap();
    let text = String::from_utf8_lossy(&chunk);
    assert!(
        text.contains("revision") || text.contains("data:"),
        "{text}"
    );

    srv.ct.cancel();
}

#[tokio::test]
async fn auth_server_hides_the_dashboard_unless_explicit() {
    let off = serve(HttpOptions {
        auth: true,
        ..Default::default()
    })
    .await;
    let http = client();
    // Unknown paths on an --auth server hit the auth fallback first, so a missing
    // dashboard looks like any other unauthenticated request. A valid token then
    // gets 404: the routes are not mounted.
    let token = off
        .store
        .create_token("alice", &["/work/*".into()], Some("dash".into()), None)
        .unwrap();
    for path in ["/ui", "/api/dash", "/api/dash/activity", "/api/dash/stream"] {
        let anon = http
            .get(format!("{}{path}", off.base))
            .send()
            .await
            .unwrap();
        assert_eq!(anon.status(), 401, "{path}");
        let authed = http
            .get(format!("{}{path}", off.base))
            .header("authorization", format!("Bearer {}", token.secret))
            .send()
            .await
            .unwrap();
        assert_eq!(authed.status(), 404, "{path} must not be mounted");
        let body = authed.text().await.unwrap();
        assert!(
            !body.contains("From one agent to another"),
            "{path}: {body}"
        );
    }
    let post = http
        .post(format!("{}/api/dash/requests/nope/expire", off.base))
        .header("authorization", format!("Bearer {}", token.secret))
        .header("content-type", "application/json")
        .header("x-another-confirm", "1")
        .body(r#"{"confirm":true}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(post.status(), 404);
    let activity = http
        .get(format!("{}/activity", off.base))
        .send()
        .await
        .unwrap();
    assert_eq!(activity.status(), 401, "auth on /activity is unchanged");
    let health: Value = serde_json::from_str(
        &http
            .get(format!("{}/health", off.base))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(health["dashboard"], "off");
    assert_eq!(health["auth"], true);
    off.ct.cancel();

    let redacted = serve(HttpOptions {
        auth: true,
        dashboard: DashboardMode::Redacted,
        ..Default::default()
    })
    .await;
    let ui = http
        .get(format!("{}/ui", redacted.base))
        .send()
        .await
        .unwrap();
    assert_eq!(ui.status(), 200);
    let snap: Value = serde_json::from_str(
        &http
            .get(format!("{}/api/dash", redacted.base))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(snap["access"]["mode"], "redacted");
    assert_eq!(snap["access"]["bodies"], false);
    assert_eq!(snap["access"]["actions"], false);
    let blob = snap.to_string();
    assert!(
        !blob.contains("SECRET BODY"),
        "an auth dashboard must not carry bodies: {blob}"
    );
    let search: Value = serde_json::from_str(
        &http
            .get(format!(
                "{}/api/dash/activity?q=SECRET%20BODY",
                redacted.base
            ))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(search["items"].as_array().unwrap().is_empty());
    let request_id = snap["requests"][0]["request_id"].as_str().unwrap();
    let act = http
        .post(format!(
            "{}/api/dash/requests/{request_id}/expire",
            redacted.base
        ))
        .header("content-type", "application/json")
        .header("x-another-confirm", "1")
        .body(r#"{"confirm":true}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(act.status(), 403);
    assert_eq!(
        http.get(format!("{}/activity", redacted.base))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    redacted.ct.cancel();
}

#[tokio::test]
async fn auth_dashboard_keeps_loopback_host_when_a_tunnel_host_is_allowed() {
    let srv = serve(HttpOptions {
        auth: true,
        dashboard: DashboardMode::Redacted,
        allowed_hosts: vec!["tunnel.example".into()],
        oauth: Some(another::oauth_http::parse_public_url("https://tunnel.example").unwrap()),
        ..Default::default()
    })
    .await;
    let authority = srv.base.trim_start_matches("http://");
    let health = raw_status(
        authority,
        "GET /health HTTP/1.1\r\nHost: tunnel.example\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(
        health, 200,
        "the tunnel host is allowed on the rest of the server"
    );
    for path in ["/ui", "/api/dash", "/api/dash/stream"] {
        let status = raw_status(
            authority,
            &format!("GET {path} HTTP/1.1\r\nHost: tunnel.example\r\nConnection: close\r\n\r\n"),
        )
        .await;
        assert_eq!(status, 403, "{path} must refuse the tunnel host");
    }
    let page = raw_status(
        authority,
        &format!("GET /ui HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert_eq!(page, 200);
    let origin = raw_status(
        authority,
        &format!(
            "POST /api/dash/holds/nope/release HTTP/1.1\r\nHost: {authority}\r\nOrigin: https://tunnel.example\r\nContent-Type: application/json\r\nContent-Length: 16\r\nX-Another-Confirm: 1\r\nConnection: close\r\n\r\n{{\"confirm\":true}}"
        ),
    )
    .await;
    assert_eq!(origin, 403, "the public origin is not a dashboard origin");
    srv.ct.cancel();

    // Actions exist only without `--auth`. The public origin still must not
    // pass, even though the edge guard accepts it for the rest of the server.
    let local = serve(HttpOptions {
        dashboard: DashboardMode::Local,
        allowed_hosts: vec!["tunnel.example".into()],
        oauth: Some(another::oauth_http::parse_public_url("https://tunnel.example").unwrap()),
        ..Default::default()
    })
    .await;
    let local_authority = local.base.trim_start_matches("http://");
    let public_origin = raw_status(
        local_authority,
        &format!(
            "POST /api/dash/holds/nope/release HTTP/1.1\r\nHost: {local_authority}\r\nOrigin: https://tunnel.example\r\nContent-Type: application/json\r\nContent-Length: 16\r\nX-Another-Confirm: 1\r\nConnection: close\r\n\r\n{{\"confirm\":true}}"
        ),
    )
    .await;
    assert_eq!(public_origin, 403);
    let bound_origin = raw_status(
        local_authority,
        &format!(
            "POST /api/dash/holds/nope/release HTTP/1.1\r\nHost: {local_authority}\r\nOrigin: http://{local_authority}\r\nContent-Type: application/json\r\nContent-Length: 16\r\nX-Another-Confirm: 1\r\nConnection: close\r\n\r\n{{\"confirm\":true}}"
        ),
    )
    .await;
    assert_eq!(bound_origin, 404, "the exact bound origin is accepted");
    local.ct.cancel();
}

#[test]
fn resolve_matrix_matches_the_loopback_rule() {
    assert_eq!(
        resolve_dashboard(true, false, None).unwrap(),
        DashboardMode::Local
    );
    assert_eq!(
        resolve_dashboard(true, true, None).unwrap(),
        DashboardMode::Off
    );
    assert_eq!(
        resolve_dashboard(false, true, None).unwrap(),
        DashboardMode::Off
    );
    assert!(resolve_dashboard(false, true, Some(true)).is_err());
    assert_eq!(
        resolve_dashboard(true, true, Some(true)).unwrap(),
        DashboardMode::Redacted
    );
}

#[tokio::test]
async fn dashboard_command_refuses_remote_bind_and_serves_loopback() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let bad = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["dashboard", "--no-open", "--bind", "0.0.0.0:9"])
        .env("ANOTHER_HOME", home)
        .env_remove("ANOTHER_LOG")
        .output()
        .unwrap();
    assert!(!bad.status.success());
    let err = String::from_utf8_lossy(&bad.stderr);
    assert!(
        err.contains("loopback"),
        "remote bind must be refused: {err}"
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["--json", "dashboard", "--no-open", "--bind", "127.0.0.1:0"])
        .env("ANOTHER_HOME", home)
        .env_remove("ANOTHER_LOG")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let line = tokio::task::spawn_blocking(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        line
    })
    .await
    .unwrap();
    let info: Value = serde_json::from_str(line.trim()).unwrap();
    let url = info["url"].as_str().unwrap().to_string();
    assert!(url.starts_with("http://127.0.0.1:"));
    assert!(url.ends_with("/ui"));
    let authority = url.trim_start_matches("http://").trim_end_matches("/ui");
    let foreign = raw_status(
        authority,
        "GET /ui HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(foreign, 403, "another dashboard refuses a foreign Host");
    let rebound = raw_status(
        authority,
        "POST /api/dash/requests/nope/expire HTTP/1.1\r\nHost: evil.example\r\nOrigin: http://evil.example\r\nContent-Type: application/json\r\nContent-Length: 16\r\nX-Another-Confirm: 1\r\nConnection: close\r\n\r\n{\"confirm\":true}",
    )
    .await;
    assert_eq!(rebound, 403, "another dashboard refuses a rebinding Origin");
    let page = client().get(&url).send().await.unwrap();
    assert_eq!(page.status(), 200);
    assert!(page.text().await.unwrap().contains("Another"));
    let _ = child.kill();
    let _ = child.wait();
}
