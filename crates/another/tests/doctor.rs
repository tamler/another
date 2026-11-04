//! `another doctor` end to end: a healthy setup passes; a missing store, an unreachable
//! server, a port taken by something else, a server for another home, port conflicts, lapsed
//! resources, scopeless and expiring tokens, missing hooks and untrusted Codex hooks are each
//! reported with a fix. Temporary homes and ephemeral loopback ports only.

use std::net::SocketAddr;
use std::path::Path;
use std::process::{Command, Stdio};

use another::serve::{http_router, HttpOptions};
use another_core::{
    LeaseMode, Program, RegisterAgent, RegisterResource, ResourceKind, ResourceRef, Store,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

fn doctor(another_home: &Path, user_home: &Path, server: &str) -> (bool, Value) {
    let out = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["--json", "doctor", "--server", server])
        .env("ANOTHER_HOME", another_home)
        .env("HOME", user_home)
        .env_remove("ANOTHER_SERVER")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (
        out.status.success(),
        serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout))),
    )
}

fn levels(r: &Value, area: &str) -> Vec<String> {
    r["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["area"] == area)
        .map(|c| c["level"].as_str().unwrap().to_string())
        .collect()
}

fn find<'a>(r: &'a Value, area: &str, level: &str) -> &'a Value {
    r["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["area"] == area && c["level"] == level)
        .unwrap_or_else(|| panic!("no {level} {area} in {r}"))
}

async fn serve(app: axum::Router) -> (String, CancellationToken) {
    let ct = CancellationToken::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    assert_ne!(addr.port(), 7466);
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
    (format!("http://{addr}"), ct)
}

async fn another_server(store: &Store) -> (String, CancellationToken) {
    let ct = CancellationToken::new();
    let app = http_router(store.clone(), ct.clone(), &HttpOptions::default());
    let (url, stop) = serve(app).await;
    (url, stop)
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_reports_each_problem_with_a_fix() {
    let dir = tempfile::tempdir().unwrap();
    let (ah, home) = (dir.path().join("another"), dir.path().join("home"));
    std::fs::create_dir_all(&home).unwrap();

    // No store yet, no server: both fail, and the exit status says so.
    let dead = "http://127.0.0.1:9"; // discard port: nothing listens
    let (ok, r) = tokio::task::spawn_blocking({
        let (ah, home) = (ah.clone(), home.clone());
        move || doctor(&ah, &home, dead)
    })
    .await
    .unwrap();
    assert!(!ok);
    assert!(find(&r, "store", "fail")["fix"]
        .as_str()
        .unwrap()
        .contains("another init"));
    assert!(find(&r, "server", "fail")["fix"]
        .as_str()
        .unwrap()
        .contains("another serve"));

    // A healthy setup: store, its own server, a scoped token, Codex hooks trusted.
    std::fs::create_dir_all(&ah).unwrap();
    let store = Store::open(&ah).unwrap();
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
    store
        .create_token("claude@shop", &["/work/shop".into()], None, None)
        .unwrap();
    let (url, stop) = another_server(&store).await;
    let codex = home.join(".codex");
    std::fs::create_dir_all(&codex).unwrap();
    let install = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["hook", "install", "--client", "codex"])
        .env("ANOTHER_HOME", &ah)
        .env("HOME", &home)
        .output()
        .unwrap();
    assert!(install.status.success());
    // The installer keeps the `codex exec` trust caveat.
    let said = String::from_utf8_lossy(&install.stdout).to_string()
        + &String::from_utf8_lossy(&install.stderr);
    assert!(
        said.contains("/hooks") && said.contains("codex exec"),
        "{said}"
    );

    let check = |url: String| {
        let (ah, home) = (ah.clone(), home.clone());
        tokio::task::spawn_blocking(move || doctor(&ah, &home, &url))
    };
    let (_, r) = check(url.clone()).await.unwrap();
    assert_eq!(levels(&r, "store"), ["pass"]);
    assert_eq!(levels(&r, "server"), ["pass"]);
    assert_eq!(levels(&r, "tokens"), ["pass"]);
    assert_eq!(levels(&r, "hooks:codex"), ["pass"]);
    // Untrusted until Codex records trust: a warning with the /hooks and codex exec fix.
    let trust = find(&r, "hooks:codex-trust", "warn");
    let fix = trust["fix"].as_str().unwrap();
    assert!(
        fix.contains("/hooks") && fix.contains("codex exec"),
        "{fix}"
    );
    // Once Codex has recorded trust for each of Another's hooks, it passes.
    let hooks_json = codex.join("hooks.json").display().to_string();
    let mut cfg = String::new();
    for (i, ev) in ["session_start", "user_prompt_submit", "session_end"]
        .iter()
        .enumerate()
    {
        cfg.push_str(&format!(
            "[hooks.state.\"{hooks_json}:{ev}:{i}:0\"]\ntrusted_hash = \"sha256:00\"\n\n"
        ));
    }
    std::fs::write(codex.join("config.toml"), cfg).unwrap();
    let (ok, r) = check(url.clone()).await.unwrap();
    assert_eq!(levels(&r, "hooks:codex-trust"), ["pass"], "{r}");
    assert!(ok, "warnings don't fail: {r}");
    // Claude Code isn't set up here: skipped, not failed.
    assert_eq!(levels(&r, "hooks:claude"), ["skip"]);
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    let (_, r) = check(url.clone()).await.unwrap();
    assert!(find(&r, "hooks:claude", "warn")["fix"]
        .as_str()
        .unwrap()
        .contains("another hook install --client claude"));

    // Problems: a token that reaches no project and one about to expire, two resources on
    // one port, a lapsed resource.
    store
        .create_token("claude@shop", &["/work/nowhere/**".into()], None, None)
        .unwrap();
    store
        .create_token("claude@shop", &["/work/shop".into()], None, Some(3600))
        .unwrap();
    let res = |name: &str, port: u16| RegisterResource {
        agent: "claude@shop".into(),
        kind: ResourceKind::DevServer,
        name: name.into(),
        endpoint: None,
        port: Some(port),
        pid: None,
        project: Some("shop".into()),
        purpose: None,
        mode: LeaseMode::Shared,
        ttl_seconds: Some(3600),
        metadata: None,
    };
    store.register_resource(res("vite-a", 5173)).unwrap();
    store.register_resource(res("vite-b", 5173)).unwrap();
    store.register_resource(res("old", 8080)).unwrap();
    store
        .release_resource(
            "claude@shop",
            &ResourceRef::parse("dev_server:old").unwrap(),
            false,
        )
        .unwrap();
    let (ok, r) = check(url.clone()).await.unwrap();
    assert!(!ok, "a port conflict fails");
    let ports = find(&r, "ports", "fail");
    assert!(
        ports["detail"].as_str().unwrap().contains("5173"),
        "{ports}"
    );
    assert!(ports["fix"].is_string());
    assert!(find(&r, "resources", "warn")["fix"]
        .as_str()
        .unwrap()
        .contains("another orphans"));
    let tokens: Vec<&str> = r["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["area"] == "tokens" && c["level"] == "warn")
        .map(|c| c["detail"].as_str().unwrap())
        .collect();
    assert!(
        tokens
            .iter()
            .any(|t| t.contains("matches no registered project")),
        "{tokens:?}"
    );
    assert!(
        tokens.iter().any(|t| t.contains("expires within 3 days")),
        "{tokens:?}"
    );

    // The server port answered by something else, or by another home's server.
    let (other_url, other_stop) =
        serve(axum::Router::new().route("/health", axum::routing::get(|| async { "hello" }))).await;
    let (_, r) = check(other_url).await.unwrap();
    assert!(find(&r, "server", "fail")["detail"]
        .as_str()
        .unwrap()
        .contains("not Another"));
    let other_dir = tempfile::tempdir().unwrap();
    let other_store = Store::open(other_dir.path()).unwrap();
    let (theirs, their_stop) = another_server(&other_store).await;
    let (_, r) = check(theirs).await.unwrap();
    assert!(find(&r, "server", "warn")["detail"]
        .as_str()
        .unwrap()
        .contains("another Another home"));

    // Human output: one line per check, a fix line under each problem, a tally.
    let text = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["doctor", "--server", &url])
        .env("ANOTHER_HOME", &ah)
        .env("HOME", &home)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&text.stdout).to_string();
    assert!(
        text.contains("FAIL  ports") && text.contains("        fix: "),
        "{text}"
    );
    assert!(text.trim_end().ends_with("warning(s)."), "{text}");

    for s in [stop, other_stop, their_stop] {
        s.cancel();
    }
}
