//! What `another serve` runs besides the router: the sweeper, the webhook delivery worker, and
//! the real process over HTTP and stdio, stopped the way a person stops it. Everything uses a
//! temporary ANOTHER_HOME and an ephemeral loopback port.

use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use another::push::spawn_delivery_worker;
use another::serve::{http_router, spawn_sweeper, HttpOptions};
use another_core::*;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

const NOV_21: i64 = 1_795_219_200_000;
const DAY: i64 = 86_400_000;

fn register(s: &Store, name: &str, program: Program, project: Option<&str>) {
    s.register_agent_trusted(RegisterAgent {
        name: name.into(),
        program,
        project: project.map(Into::into),
        model: None,
        description: None,
    })
    .unwrap();
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn summaries(s: &Store, agent: &str) -> usize {
    s.inbox(
        agent,
        InboxQuery {
            limit: 50,
            ..Default::default()
        },
    )
    .unwrap()
    .messages
    .len()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sweeper_expires_holds_sends_the_summary_and_stops_when_cancelled() {
    let (clock, now) = Clock::manual(NOV_21);
    let s = Store::open_in_memory(clock).unwrap();
    s.ensure_project("t", "/work/shop").unwrap();
    register(&s, "pat", Program::Human, None);
    register(&s, "claude", Program::Claude, Some("/work/shop"));
    let held = s
        .acquire_holds(AcquireHolds {
            agent: "claude@shop".into(),
            project: "shop".into(),
            paths: vec!["src/api.rs".into()],
            ttl_seconds: Some(60),
            exclusive: true,
            reason: None,
            lane: None,
        })
        .unwrap();
    assert_eq!(held.granted.len(), 1);
    s.set_summary_schedule(Some("00:00"), Some("mail:pat"))
        .unwrap();
    now.fetch_add(120_000, Ordering::SeqCst);

    let ct = CancellationToken::new();
    spawn_sweeper(s.clone(), Duration::from_millis(10), ct.clone());
    until("the daily summary", || summaries(&s, "pat") == 1).await;
    let expired = s.events(1000).unwrap();
    assert!(
        expired.iter().any(|e| e.kind == "hold.expired"),
        "{:?}",
        expired.iter().map(|e| &e.kind).collect::<Vec<_>>()
    );

    // Later sweeps on the same day send nothing more.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(summaries(&s, "pat"), 1);

    // Once cancelled, the next day's summary is never sent.
    ct.cancel();
    tokio::time::sleep(Duration::from_millis(100)).await;
    now.fetch_add(2 * DAY, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(summaries(&s, "pat"), 1, "the sweeper stopped");
}

type Received = Arc<Mutex<Vec<String>>>;

async fn serve_on_loopback(app: axum::Router, ct: CancellationToken) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    assert_ne!(addr.port(), 7466);
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { ct.cancelled_owned().await })
        .await
        .unwrap();
    });
    addr
}

async fn receiver(ct: CancellationToken) -> (String, Received) {
    let got: Received = Arc::default();
    let sink = got.clone();
    let app = axum::Router::new().route(
        "/hook",
        axum::routing::post(move |body: String| {
            let sink = sink.clone();
            async move {
                sink.lock().unwrap().push(body);
                axum::http::StatusCode::OK
            }
        }),
    );
    let addr = serve_on_loopback(app, ct).await;
    (format!("http://{addr}/hook"), got)
}

fn lane_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path()).unwrap();
    s.ensure_project("t", "/work/shop").unwrap();
    register(&s, "manager", Program::GrokBot, None);
    register(&s, "claude", Program::Claude, Some("/work/shop"));
    (dir, s)
}

fn new_lane(s: &Store, name: &str) {
    s.create_lane(
        NewLane {
            agent: "manager".into(),
            name: name.into(),
            project: "shop".into(),
            lead: "claude@shop".into(),
            charter: "charter".into(),
            status: Some(LaneStatus::Working),
        },
        None,
    )
    .unwrap();
}

fn hook(s: &Store, url: &str) -> String {
    s.create_webhook(
        NewWebhook {
            url: url.into(),
            filters: vec![WebhookFilter::LaneStatus],
            project_globs: None,
            description: None,
        },
        None,
    )
    .unwrap()
    .webhook
    .webhook_id
}

#[tokio::test(flavor = "multi_thread")]
async fn the_delivery_worker_sends_webhooks_and_records_a_dead_receiver() {
    let (_dir, s) = lane_store();
    let stop = CancellationToken::new();
    let (url, received) = receiver(stop.clone()).await;
    let good = hook(&s, &url);

    // Nothing listens here: the port was just released.
    let dead_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let bad = hook(&s, &format!("http://127.0.0.1:{dead_port}/hook"));

    new_lane(&s, "api");
    let ct = CancellationToken::new();
    spawn_delivery_worker(s.clone(), ct.clone());
    until("the delivery", || !received.lock().unwrap().is_empty()).await;
    assert!(
        received.lock().unwrap()[0].contains("api"),
        "{:?}",
        received.lock().unwrap()
    );

    until("the failed attempt", || {
        s.webhook_deliveries(Some(&bad), None, 10)
            .unwrap()
            .iter()
            .any(|d| d.attempts > 0)
    })
    .await;
    let failed = s.webhook_deliveries(Some(&bad), None, 10).unwrap();
    assert_ne!(failed[0].status, DeliveryStatus::Delivered, "{failed:?}");
    assert!(failed[0].last_error.is_some(), "{failed:?}");
    let sent = s.webhook_deliveries(Some(&good), None, 10).unwrap();
    assert_eq!(sent[0].status, DeliveryStatus::Delivered, "{sent:?}");

    // Once cancelled, new events stay queued.
    ct.cancel();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let before = received.lock().unwrap().len();
    new_lane(&s, "web");
    tokio::time::sleep(another::push::DELIVERY_EVERY + Duration::from_millis(500)).await;
    assert_eq!(received.lock().unwrap().len(), before, "the worker stopped");
    stop.cancel();
}

/// A store behind a real authenticated router, with a bearer token for the shop project.
async fn authed() -> (
    tempfile::TempDir,
    Store,
    String,
    AuthToken,
    CancellationToken,
) {
    let (dir, s) = lane_store();
    let ct = CancellationToken::new();
    let app = http_router(
        s.clone(),
        ct.clone(),
        &HttpOptions {
            auth: true,
            ..Default::default()
        },
    );
    let addr = serve_on_loopback(app, ct.clone()).await;
    let t = s
        .create_token("manager", &["/work/shop".into()], None, None)
        .unwrap();
    let token = AuthToken {
        id: t.token.token_id.clone(),
        secret: t.secret.clone(),
    };
    (dir, s, format!("http://{addr}"), token, ct)
}

struct AuthToken {
    id: String,
    secret: String,
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_event_kinds_and_cursors_are_refused_and_audited() {
    let (dir, s, base, token, ct) = authed().await;
    let http = reqwest::Client::new();
    for path in [
        "/events/poll?kinds=nonsense&timeout=0",
        "/events/stream?kinds=nonsense",
        "/events/stream?since=not-a-cursor",
    ] {
        let resp = http
            .get(format!("{base}{path}"))
            .bearer_auth(&token.secret)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "{path}");
        let body: Value = serde_json::from_str(&resp.text().await.unwrap()).unwrap();
        assert!(body["error"]["message"].is_string(), "{path}: {body}");
    }
    let log = s.audit_log(20).unwrap();
    let errors: Vec<_> = log
        .iter()
        .filter(|e| e.outcome == "error")
        .map(|e| e.operation.as_str())
        .collect();
    assert_eq!(
        errors
            .iter()
            .filter(|op| **op == "GET /events/stream")
            .count(),
        2,
        "{errors:?}"
    );
    assert!(errors.contains(&"GET /events/poll"), "{errors:?}");
    // The person reading the audit log sees the calls and where they came from.
    let audit = another(dir.path()).args(["audit"]).output().unwrap();
    let audit = String::from_utf8_lossy(&audit.stdout);
    assert!(audit.contains("GET /events/stream"), "{audit}");
    assert!(audit.contains("[127.0.0.1"), "{audit}");
    ct.cancel();
}

/// Takes as long as the server's recheck interval (30 s): a revoked token must not keep a
/// stream it already opened.
#[tokio::test(flavor = "multi_thread")]
async fn an_open_stream_ends_after_its_token_is_revoked() {
    let (_dir, s, base, token, ct) = authed().await;
    let mut resp = reqwest::Client::new()
        .get(format!("{base}/events/stream"))
        .bearer_auth(&token.secret)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    s.revoke_token(&token.id).unwrap();

    let ended = tokio::time::timeout(Duration::from_secs(45), async {
        while resp.chunk().await.unwrap().is_some() {}
    })
    .await;
    assert!(ended.is_ok(), "the stream outlived its revoked token");
    ct.cancel();
}

// ---------------------------------------------------------------------------------------
// The real process

/// Kills the child if a failed assertion leaves it running.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn another(home: &std::path::Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_another"));
    c.env("ANOTHER_HOME", home)
        .env_remove("ANOTHER_AGENT")
        .env_remove("ANOTHER_SERVER")
        .env_remove("ANOTHER_LOG");
    c
}

fn wait_for_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "the server did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Read the serve banner, which names the real port, on a thread so a silent server can't hang
/// the test.
fn banner(child: &mut Child) -> String {
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if line.contains("streamable HTTP") {
                let _ = tx.send(line);
            }
        }
    });
    rx.recv_timeout(Duration::from_secs(15))
        .expect("serve printed its banner")
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn serve_over_http_answers_and_exits_cleanly_on_interrupt() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = Running(
        another(dir.path())
            .args([
                "serve",
                "--bind",
                "127.0.0.1:0",
                "--auth",
                "--sweep-secs",
                "1",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let line = banner(&mut server.0);
    assert!(line.contains("bearer token required"), "{line}");
    let base = line
        .split("http://")
        .nth(1)
        .and_then(|rest| rest.split("/mcp").next())
        .unwrap_or_else(|| panic!("no address in {line}"));
    let base = format!("http://{base}");

    let http = reqwest::Client::new();
    assert_eq!(
        http.get(format!("{base}/health"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        http.get(format!("{base}/activity"))
            .send()
            .await
            .unwrap()
            .status(),
        401,
        "--auth reached the router"
    );

    let pid = server.0.id().to_string();
    let signalled = Command::new("kill")
        .args(["-INT", &pid])
        .status()
        .unwrap()
        .success();
    assert!(signalled);
    assert!(wait_for_exit(&mut server.0).success());
}

#[test]
fn serve_reports_a_port_that_is_already_taken() {
    let dir = tempfile::tempdir().unwrap();
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap().to_string();
    let out = another(dir.path())
        .args(["serve", "--bind", &addr])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains(&format!("binding {addr}")), "{err}");
}

#[test]
fn serve_over_stdio_speaks_mcp_and_exits_when_its_client_goes_away() {
    let dir = tempfile::tempdir().unwrap();
    let mut server = Running(
        another(dir.path())
            .args(["serve", "--stdio"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut stdin = server.0.stdin.take().unwrap();
    let stdout = server.0.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let mut send = |v: Value| writeln!(stdin, "{v}").unwrap();
    send(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "0" }
        }
    }));
    let reply = |id: i64| -> Value {
        loop {
            let line = rx
                .recv_timeout(Duration::from_secs(15))
                .expect("a reply on stdout");
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["id"] == id {
                return v;
            }
        }
    };
    assert!(reply(1)["result"]["serverInfo"].is_object());
    send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    send(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
    let tools = reply(2);
    assert!(
        tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "lane_report"),
        "{tools}"
    );

    drop(stdin);
    assert!(wait_for_exit(&mut server.0).success());
}
