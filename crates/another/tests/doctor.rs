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
    // Trust is matched per Another hook entry. Add an unrelated hook of the user's to the
    // file (first in each event), trust only that: Another's are still untrusted.
    let hooks_path = codex.join("hooks.json");
    let hooks_json = hooks_path.display().to_string();
    let mut hj: Value =
        serde_json::from_str(&std::fs::read_to_string(&hooks_path).unwrap()).unwrap();
    let events: Vec<String> = hj["hooks"].as_object().unwrap().keys().cloned().collect();
    for ev in &events {
        hj["hooks"][ev].as_array_mut().unwrap().insert(
            0,
            serde_json::json!({ "hooks": [{ "type": "command", "command": "echo mine" }] }),
        );
    }
    std::fs::write(&hooks_path, hj.to_string()).unwrap();
    let snake = |e: &str| {
        let mut o = String::new();
        for (i, c) in e.chars().enumerate() {
            if c.is_ascii_uppercase() {
                if i > 0 {
                    o.push('_');
                }
                o.push(c.to_ascii_lowercase());
            } else {
                o.push(c);
            }
        }
        o
    };
    let trust = |group: usize| {
        events
            .iter()
            .map(|ev| {
                format!(
                    "[hooks.state.\"{hooks_json}:{}:{group}:0\"]\ntrusted_hash = \"sha256:00\"\n\n",
                    snake(ev)
                )
            })
            .collect::<String>()
    };
    std::fs::write(codex.join("config.toml"), trust(0)).unwrap();
    let (_, r) = check(url.clone()).await.unwrap();
    assert_eq!(
        levels(&r, "hooks:codex-trust"),
        ["warn"],
        "other hooks' trust doesn't count for Another's: {r}"
    );
    // Once Codex has recorded trust for each of Another's own entries (group 1 now), it passes.
    std::fs::write(codex.join("config.toml"), trust(0) + &trust(1)).unwrap();
    let (ok, r) = check(url.clone()).await.unwrap();
    assert_eq!(levels(&r, "hooks:codex-trust"), ["pass"], "{r}");
    assert!(ok, "warnings don't fail: {r}");
    // A command that only mentions the wrapper isn't one of Another's hooks, so it needs no trust.
    let saved_hooks = std::fs::read_to_string(&hooks_path).unwrap();
    for ev in &events {
        hj["hooks"][ev].as_array_mut().unwrap().push(serde_json::json!({
            "hooks": [{ "type": "command", "command": "echo another-hook.sh" }]
        }));
    }
    std::fs::write(&hooks_path, hj.to_string()).unwrap();
    let (_, r) = check(url.clone()).await.unwrap();
    assert_eq!(levels(&r, "hooks:codex-trust"), ["pass"], "{r}");
    std::fs::write(&hooks_path, saved_hooks).unwrap();
    // A missing wrapper fails, even though the config still names it.
    let wrapper = ah.join("hooks").join("another-hook.sh");
    let saved = std::fs::read(&wrapper).unwrap();
    std::fs::remove_file(&wrapper).unwrap();
    let (ok, r) = check(url.clone()).await.unwrap();
    assert!(!ok);
    let f = find(&r, "hooks:codex", "fail");
    assert!(
        f["detail"]
            .as_str()
            .unwrap()
            .contains("missing or not executable"),
        "{f}"
    );
    assert!(f["fix"]
        .as_str()
        .unwrap()
        .contains("another hook install --client codex"));
    std::fs::write(&wrapper, saved).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
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
    // Other programs' health checks also say ok; Another's always names its node.
    let (bare_url, bare_stop) = serve(axum::Router::new().route(
        "/health",
        axum::routing::get(|| async { axum::Json(serde_json::json!({ "ok": true })) }),
    ))
    .await;
    let (_, r) = check(bare_url).await.unwrap();
    assert!(find(&r, "server", "fail")["detail"]
        .as_str()
        .unwrap()
        .contains("not Another"));
    bare_stop.cancel();
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

#[tokio::test(flavor = "multi_thread")]
async fn unreadable_hook_configs_fail_instead_of_passing() {
    let dir = tempfile::tempdir().unwrap();
    let (ah, home) = (dir.path().join("another"), dir.path().join("home"));
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    std::fs::create_dir_all(&ah).unwrap();
    Store::open(&ah).unwrap();
    let hooks = home.join(".codex").join("hooks.json");
    let run = || {
        let (ah, home) = (ah.clone(), home.clone());
        tokio::task::spawn_blocking(move || doctor(&ah, &home, "http://127.0.0.1:9"))
    };
    // An edit cut short: still mentions the wrapper, no longer JSON.
    std::fs::write(
        &hooks,
        "{\"hooks\": {\"SessionStart\": [{\"hooks\": [{\"type\": \"command\", \"command\": \"'/x/another-hook.sh' session-start",
    )
    .unwrap();
    let (ok, r) = run().await.unwrap();
    assert!(!ok);
    let f = find(&r, "hooks:codex", "fail");
    assert!(
        f["detail"].as_str().unwrap().contains("not valid JSON"),
        "{f}"
    );
    assert!(f["fix"]
        .as_str()
        .unwrap()
        .contains("another hook install --client codex"));
    assert!(
        levels(&r, "hooks:codex-trust").is_empty(),
        "no trust verdict on a broken file"
    );
    // Valid JSON that only mentions the wrapper outside any hook command.
    std::fs::write(
        &hooks,
        r#"{"note": "another-hook.sh was here", "hooks": {}}"#,
    )
    .unwrap();
    let (_, r) = run().await.unwrap();
    assert!(find(&r, "hooks:codex", "fail")["detail"]
        .as_str()
        .unwrap()
        .contains("no hook command runs it"));
    for config in [
        r#"{"note":{"command":"see another-hook.sh"},"hooks":{}}"#,
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo another-hook.sh"}]}]}}"#,
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"'/x/another-hook.sh"}]}]}}"#,
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"'/x/another-hook.sh'.bak"}]}]}}"#,
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"another-hook.sh session-start"}]}]}}"#,
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"./another-hook.sh session-start"}]}]}}"#,
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"/x/not-another-hook.sh session-start"}]}]}}"#,
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"\u00a0'/x/another-hook.sh'"}]}]}}"#,
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"'/x/another-hook.sh'\u00a0foo"}]}]}}"#,
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"'/x/another-hook.sh/'"}]}]}}"#,
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"/x/another-hook.sh/."}]}]}}"#,
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"\"/x/another-hook.sh\""}]}]}}"#,
    ] {
        std::fs::write(&hooks, config).unwrap();
        let (_, r) = run().await.unwrap();
        assert!(find(&r, "hooks:codex", "fail")["detail"]
            .as_str()
            .unwrap()
            .contains("no hook command runs it"));
        assert!(levels(&r, "hooks:codex-trust").is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn installed_hook_commands_are_required_for_each_client() {
    let dir = tempfile::tempdir().unwrap();
    let (ah, home) = (
        dir.path().join("o'brien space/another"),
        dir.path().join("home"),
    );
    std::fs::create_dir_all(&home).unwrap();
    Store::open(&ah).unwrap();
    for (client, config) in [
        ("claude", ".claude/settings.json"),
        ("codex", ".codex/hooks.json"),
        ("grok", ".grok/hooks/another.json"),
    ] {
        let install = Command::new(env!("CARGO_BIN_EXE_another"))
            .args(["hook", "install", "--client", client])
            .env("ANOTHER_HOME", &ah)
            .env("HOME", &home)
            .output()
            .unwrap();
        assert!(install.status.success(), "{client}: {install:?}");
        let run = || {
            let (ah, home) = (ah.clone(), home.clone());
            tokio::task::spawn_blocking(move || doctor(&ah, &home, "http://127.0.0.1:9"))
        };
        let area = format!("hooks:{client}");
        let (_, healthy) = run().await.unwrap();
        assert_eq!(levels(&healthy, &area), ["pass"], "{client}: {healthy}");
        let installed: Value =
            serde_json::from_str(&std::fs::read_to_string(home.join(config)).unwrap()).unwrap();
        let command = installed["hooks"]
            .as_object()
            .unwrap()
            .values()
            .flat_map(|groups| groups.as_array().unwrap())
            .flat_map(|group| group["hooks"].as_array().unwrap())
            .find_map(|hook| hook["command"].as_str())
            .unwrap();
        let outside = serde_json::json!({"disabled": {"command": command}, "hooks": {}});
        std::fs::write(home.join(config), outside.to_string()).unwrap();
        let (_, broken) = run().await.unwrap();
        assert_eq!(levels(&broken, &area), ["fail"], "{client}: {broken}");
        assert!(levels(&broken, "hooks:codex-trust").is_empty());
    }
}
