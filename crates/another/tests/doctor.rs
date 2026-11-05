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
        hj["hooks"][ev]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
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

// ---------------------------------------------------------------------------------------
// Edge cases from the second coverage pass (`cargo llvm-cov`). Tests only. A test that shows
// a bug asserts today's behaviour and points at BUGS-FOUND.md.

const DEAD: &str = "http://127.0.0.1:9"; // discard port: nothing listens

struct Homes {
    _dir: tempfile::TempDir,
    ah: std::path::PathBuf,
    home: std::path::PathBuf,
}

/// An initialized Another home and an empty user home (no client set up).
fn homes() -> Homes {
    let dir = tempfile::tempdir().unwrap();
    let (ah, home) = (dir.path().join("another"), dir.path().join("home"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&ah).unwrap();
    Store::open(&ah).unwrap();
    Homes {
        _dir: dir,
        ah,
        home,
    }
}

fn install_hooks(h: &Homes, client: &str) {
    let out = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["hook", "install", "--client", client])
        .env("ANOTHER_HOME", &h.ah)
        .env("HOME", &h.home)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn agent_in_shop(store: &Store) {
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
}

fn detail<'a>(r: &'a Value, area: &str, level: &str) -> &'a str {
    find(r, area, level)["detail"].as_str().unwrap()
}

fn details(r: &Value, area: &str) -> Vec<String> {
    r["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["area"] == area)
        .map(|c| c["detail"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn a_corrupt_or_newer_database_fails_and_skips_what_needs_the_store() {
    let h = homes();
    let db = h.ah.join("another.db");

    // A database from a newer build: the schema version lives at byte 60 of the SQLite header.
    let healthy = std::fs::read(&db).unwrap();
    let mut newer = healthy.clone();
    newer[60..64].copy_from_slice(&9_999u32.to_be_bytes());
    std::fs::write(&db, &newer).unwrap();
    let (ok, r) = doctor(&h.ah, &h.home, DEAD);
    assert!(!ok);
    assert!(detail(&r, "store", "fail").contains("newer"), "{r}");
    assert!(find(&r, "store", "fail")["fix"]
        .as_str()
        .unwrap()
        .contains("newer `another`"));

    // Garbage: reported, not a crash, and the checks that need a store say why they didn't run.
    std::fs::write(&db, b"this is not a database, not even close").unwrap();
    let (ok, r) = doctor(&h.ah, &h.home, DEAD);
    assert!(!ok);
    assert!(detail(&r, "store", "fail").contains("cannot open the database"));
    for area in ["tokens", "resources"] {
        assert_eq!(levels(&r, area), ["skip"], "{r}");
        assert_eq!(detail(&r, area, "skip"), "store unavailable");
    }
    assert!(
        levels(&r, "ports").is_empty(),
        "no port verdict without a store: {r}"
    );
    // The server and client checks still run.
    assert_eq!(levels(&r, "server"), ["fail"]);
    assert_eq!(levels(&r, "hooks:codex"), ["skip"]);

    // Doctor didn't damage the bad file, so it can still be inspected.
    assert_eq!(
        std::fs::read(&db).unwrap(),
        b"this is not a database, not even close"
    );
}

#[cfg(unix)]
#[test]
fn an_unwritable_home_fails_with_the_chmod_fix() {
    use std::os::unix::fs::PermissionsExt;
    let h = homes();
    std::fs::set_permissions(&h.ah, std::fs::Permissions::from_mode(0o500)).unwrap();
    // root ignores the mode: nothing to observe there.
    let writable = std::fs::write(h.ah.join(".w"), b"x").is_ok();
    let (ok, r) = doctor(&h.ah, &h.home, DEAD);
    std::fs::set_permissions(&h.ah, std::fs::Permissions::from_mode(0o700)).unwrap();
    if writable {
        return;
    }
    assert!(!ok);
    assert!(detail(&r, "store", "fail").contains("is not writable"));
    assert!(find(&r, "store", "fail")["fix"]
        .as_str()
        .unwrap()
        .contains("chmod u+rwx"));
    assert_eq!(levels(&r, "tokens"), ["skip"]);
}

#[test]
fn repeated_runs_leave_no_probe_behind_and_agree() {
    let h = homes();
    let names = || -> Vec<String> {
        let mut n: Vec<String> = std::fs::read_dir(&h.ah)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| !n.starts_with("another.db"))
            .collect();
        n.sort();
        n
    };
    let (_, first) = doctor(&h.ah, &h.home, DEAD);
    let after_first = names();
    assert!(
        !after_first.iter().any(|n| n.starts_with(".doctor-probe")),
        "{after_first:?}"
    );
    let (_, second) = doctor(&h.ah, &h.home, DEAD);
    assert_eq!(
        first, second,
        "doctor is read-mostly: a re-run sees the same"
    );
    assert_eq!(names(), after_first);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_server_check_goes_by_what_health_answers() {
    let h = homes();
    let store = Store::open(&h.ah).unwrap();
    let check = |url: String, ah: std::path::PathBuf| {
        let home = h.home.clone();
        tokio::task::spawn_blocking(move || doctor(&ah, &home, &url))
    };
    let answering = |status: u16, body: &'static str| {
        serve(axum::Router::new().route(
            "/health",
            axum::routing::get(move || async move {
                (
                    axum::http::StatusCode::from_u16(status).unwrap(),
                    body.to_string(),
                )
            }),
        ))
    };

    // Our own server, with auth on and a trailing slash in the URL.
    let ct = CancellationToken::new();
    let app = http_router(
        store.clone(),
        ct.clone(),
        &HttpOptions {
            auth: true,
            ..Default::default()
        },
    );
    let (url, stop) = serve(app).await;
    let (_, r) = check(format!("{url}/"), h.ah.clone()).await.unwrap();
    assert!(
        detail(&r, "server", "pass").ends_with("is up, token auth on"),
        "{r}"
    );
    stop.cancel();

    // A body that says it's not healthy, an error page, an empty one: not Another.
    for (status, body) in [
        (200, r#"{"ok": false}"#),
        (200, r#"{"ok": "true"}"#),
        (500, "boom"),
        (404, ""),
        (200, "[]"),
    ] {
        let (url, stop) = answering(status, body).await;
        let (_, r) = check(url, h.ah.clone()).await.unwrap();
        assert!(
            detail(&r, "server", "fail").contains("not Another"),
            "{status} {body}: {r}"
        );
        assert!(find(&r, "server", "fail")["fix"]
            .as_str()
            .unwrap()
            .contains("--bind"));
        stop.cancel();
    }

    // Healthy but with no node id: Another always names its node, so this is something else,
    // with or without a store of our own to compare it with.
    let (url, stop) = answering(200, r#"{"ok": true}"#).await;
    let (_, r) = check(url.clone(), h.ah.clone()).await.unwrap();
    assert!(detail(&r, "server", "fail").contains("not Another"), "{r}");
    let nowhere = h.home.join("no-such-home");
    let (ok, r) = check(url, nowhere).await.unwrap();
    assert!(detail(&r, "server", "fail").contains("not Another"), "{r}");
    assert!(!ok, "the missing store still fails");
    stop.cancel();

    // A URL that can't even be requested is unreachable, not a panic.
    for bad in ["not a url", "", "http://", "ftp://127.0.0.1:9"] {
        let (ok, r) = check(bad.to_string(), h.ah.clone()).await.unwrap();
        assert!(!ok, "{bad:?}");
        assert!(
            detail(&r, "server", "fail").contains("not reachable"),
            "{bad:?}: {r}"
        );
    }
}

#[test]
fn tokens_are_judged_by_revocation_expiry_and_reach() {
    const HOUR: i64 = 3_600;
    let h = homes();
    let store = Store::open(&h.ah).unwrap();
    agent_in_shop(&store);
    let shop = ["/work/shop".to_string()];
    let run = || doctor(&h.ah, &h.home, DEAD).1;
    assert_eq!(
        detail(&run(), "tokens", "pass"),
        "no active tokens (local use only)"
    );

    // Revoked tokens don't count, not even when they reach nothing.
    let t = store
        .create_token("claude@shop", &["/nowhere/**".to_string()], None, None)
        .unwrap();
    store.revoke_token(&t.token.token_id).unwrap();
    assert_eq!(
        detail(&run(), "tokens", "pass"),
        "no active tokens (local use only)"
    );

    // Expiry window: just outside three days is quiet, just inside warns.
    let later = store
        .create_token("claude@shop", &shop, None, Some(73 * HOUR))
        .unwrap();
    assert_eq!(
        detail(&run(), "tokens", "pass"),
        "1 active token(s), each reaching at least one project"
    );
    let soon = store
        .create_token("claude@shop", &shop, None, Some(71 * HOUR))
        .unwrap();
    let r = run();
    assert_eq!(levels(&r, "tokens"), ["warn"], "{r}");
    assert!(
        detail(&r, "tokens", "warn").contains(&soon.token.token_id),
        "{r}"
    );
    assert!(!detail(&r, "tokens", "warn").contains(&later.token.token_id));
    store.revoke_token(&soon.token.token_id).unwrap();

    // Expired: a warning naming the token and how to clear it, not a failure.
    let gone = store
        .create_token("claude@shop", &shop, None, Some(1))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1_100));
    let r = run();
    assert_eq!(levels(&r, "tokens"), ["warn"], "{r}");
    let w = find(&r, "tokens", "warn");
    assert!(w["detail"].as_str().unwrap().contains("has expired"), "{w}");
    assert!(w["fix"]
        .as_str()
        .unwrap()
        .contains(&format!("another token revoke {}", gone.token.token_id)));
    store.revoke_token(&gone.token.token_id).unwrap();

    // Both problems at once: reaching nothing outranks expiring soon, one line per token.
    store
        .create_token(
            "claude@shop",
            &["/nowhere/**".to_string()],
            None,
            Some(HOUR),
        )
        .unwrap();
    let r = run();
    let lines = details(&r, "tokens");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].contains("matches no registered project") && lines[0].contains("/nowhere/**"),
        "{lines:?}"
    );
}

fn claim(store: &Store, name: &str, port: Option<u16>) {
    store
        .register_resource(RegisterResource {
            agent: "claude@shop".into(),
            kind: ResourceKind::DevServer,
            name: name.into(),
            endpoint: None,
            port,
            pid: None,
            project: Some("shop".into()),
            purpose: None,
            mode: LeaseMode::Shared,
            ttl_seconds: Some(3600),
            metadata: None,
        })
        .unwrap();
}

#[test]
fn ports_are_checked_against_each_other_and_the_server() {
    let h = homes();
    let store = Store::open(&h.ah).unwrap();
    agent_in_shop(&store);
    let run = |server: &str| doctor(&h.ah, &h.home, server).1;

    // Nothing claimed; a resource without a port isn't a claim.
    claim(&store, "worker", None);
    let r = run(DEAD);
    assert_eq!(
        detail(&r, "ports", "pass"),
        "0 claimed port(s), no conflicts"
    );
    assert_eq!(
        detail(&r, "resources", "pass"),
        "1 live resource(s), none lapsed"
    );

    // A resource on Another's own port fails, however the URL ends.
    claim(&store, "squatter", Some(9));
    for server in [DEAD.to_string(), format!("{DEAD}/")] {
        let r = run(&server);
        assert_eq!(levels(&r, "ports"), ["fail"], "{server}: {r}");
        let f = find(&r, "ports", "fail");
        assert!(
            f["detail"].as_str().unwrap().contains("server port")
                && f["detail"]
                    .as_str()
                    .unwrap()
                    .contains("dev_server:squatter"),
            "{f}"
        );
        assert!(f["fix"].as_str().unwrap().contains("--bind"));
    }
    // A URL with no port names none: no comparison, so this claim alone is fine.
    assert_eq!(levels(&run("http://127.0.0.1"), "ports"), ["pass"]);

    // Double-claimed and on the server's port: both problems are reported.
    claim(&store, "squatter2", Some(9));
    let r = run(DEAD);
    assert_eq!(levels(&r, "ports"), ["fail", "fail"], "{r}");
    let lines = details(&r, "ports");
    assert!(
        lines[0].contains("is claimed by") && lines[0].matches("dev_server:squatter").count() == 2,
        "{lines:?}"
    );
    assert!(lines[1].contains("server port"), "{lines:?}");

    // Released resources stop claiming their port.
    for name in ["squatter", "squatter2"] {
        store
            .release_resource(
                "claude@shop",
                &ResourceRef::parse(&format!("dev_server:{name}")).unwrap(),
                true,
            )
            .unwrap();
    }
    assert_eq!(levels(&run(DEAD), "ports"), ["pass"]);
}

#[test]
fn lapsed_resources_are_counted_in_full_but_listed_up_to_five() {
    let h = homes();
    let store = Store::open(&h.ah).unwrap();
    agent_in_shop(&store);
    let names: Vec<String> = (0..7).map(|i| format!("old-{i}")).collect();
    for n in &names {
        claim(&store, n, None);
        store
            .release_resource(
                "claude@shop",
                &ResourceRef::parse(&format!("dev_server:{n}")).unwrap(),
                false,
            )
            .unwrap();
    }
    let (ok, r) = doctor(&h.ah, &h.home, DEAD);
    assert!(!ok, "the dead server fails, lapsed resources only warn");
    let d = detail(&r, "resources", "warn");
    assert!(d.starts_with("7 lapsed resource(s)"), "{d}");
    let listed = names.iter().filter(|n| d.contains(n.as_str())).count();
    assert_eq!(listed, 5, "{d}");
}

#[test]
fn without_a_home_the_client_checks_are_skipped() {
    let h = homes();
    let out = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["--json", "doctor", "--server", DEAD])
        .env("ANOTHER_HOME", &h.ah)
        .env_remove("HOME")
        .env_remove("ANOTHER_SERVER")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let r: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(levels(&r, "hooks"), ["skip"], "{r}");
    assert!(detail(&r, "hooks", "skip").contains("HOME is not set"));
    assert!(levels(&r, "hooks:codex").is_empty());

    // An empty HOME is the same as none.
    let out = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["--json", "doctor", "--server", DEAD])
        .env("ANOTHER_HOME", &h.ah)
        .env("HOME", "")
        .env_remove("ANOTHER_SERVER")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let r: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(levels(&r, "hooks"), ["skip"], "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_clean_setup_says_all_checks_passed() {
    let h = homes();
    let store = Store::open(&h.ah).unwrap();
    let (url, stop) = another_server(&store).await;
    let (ah, home) = (h.ah.clone(), h.home.clone());
    let out = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_another"))
            .args(["doctor", "--server", &url])
            .env("ANOTHER_HOME", &ah)
            .env("HOME", &home)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    stop.cancel();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(text.trim_end().ends_with("All checks passed."), "{text}");
    assert!(text.contains("SKIP  hooks:claude"), "{text}");
    assert!(!text.contains("fix:"), "{text}");
}

#[test]
fn the_report_renders_every_level_and_a_tally() {
    use another::doctor::{render, Check, Level, Report};
    let c = |area: &str, level, fix: Option<&str>| Check {
        area: area.into(),
        level,
        detail: format!("{area} detail"),
        fix: fix.map(Into::into),
    };
    let report = |checks, failed, warnings| Report {
        home: "/h".into(),
        checks,
        failed,
        warnings,
    };
    let text = render(&report(
        vec![
            c("a", Level::Pass, None),
            c("b", Level::Warn, Some("do b")),
            c("c", Level::Fail, Some("do c")),
            c("d", Level::Skip, None),
        ],
        1,
        1,
    ));
    assert!(text.starts_with("another doctor (home /h)\n"), "{text}");
    for want in ["PASS  a", "WARN  b", "FAIL  c", "SKIP  d"] {
        assert!(text.contains(want), "{want}: {text}");
    }
    assert!(text.contains("  fix: do b\n") && text.contains("  fix: do c\n"));
    assert!(text.ends_with("1 failed, 1 warning(s)."), "{text}");
    // Either count alone is enough to withhold the all-clear.
    assert!(render(&report(vec![], 0, 2)).ends_with("0 failed, 2 warning(s)."));
    assert!(render(&report(vec![], 3, 0)).ends_with("3 failed, 0 warning(s)."));
    assert!(render(&report(vec![], 0, 0)).ends_with("All checks passed."));
}

#[test]
fn every_installed_client_is_reported_on_its_own() {
    let h = homes();
    for dir in [".claude", ".codex", ".grok"] {
        std::fs::create_dir_all(h.home.join(dir)).unwrap();
    }
    let (_, r) = doctor(&h.ah, &h.home, DEAD);
    for area in ["hooks:claude", "hooks:codex", "hooks:grok"] {
        assert_eq!(levels(&r, area), ["warn"], "{area}: {r}");
    }
    assert!(
        levels(&r, "hooks:codex-trust").is_empty(),
        "no trust verdict without hooks"
    );
    install_hooks(&h, "claude");
    install_hooks(&h, "grok");
    let (_, r) = doctor(&h.ah, &h.home, DEAD);
    assert!(detail(&r, "hooks:claude", "pass").starts_with("Claude Code hooks installed"));
    assert!(detail(&r, "hooks:grok", "pass").starts_with("Grok hooks installed"));
    assert_eq!(levels(&r, "hooks:codex"), ["warn"], "{r}");
    assert!(
        levels(&r, "hooks:codex-trust").is_empty(),
        "trust is only about Codex hooks that are installed"
    );

    // A client's config path taken by a directory can't be read. Today that reads as not
    // installed; PR #25 makes it a failure (BUGS-FOUND.md #6). Neither side passes it.
    std::fs::create_dir_all(h.home.join(".codex/hooks.json")).unwrap();
    let (_, r) = doctor(&h.ah, &h.home, DEAD);
    assert_ne!(levels(&r, "hooks:codex"), ["pass"], "{r}");
}

#[cfg(unix)]
#[test]
fn a_wrapper_must_be_an_executable_file() {
    use std::os::unix::fs::PermissionsExt;
    let h = homes();
    std::fs::create_dir_all(h.home.join(".codex")).unwrap();
    install_hooks(&h, "codex");
    let wrapper = h.ah.join("hooks").join("another-hook.sh");

    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o644)).unwrap();
    let (ok, r) = doctor(&h.ah, &h.home, DEAD);
    assert!(!ok);
    assert!(detail(&r, "hooks:codex", "fail").contains("missing or not executable"));
    assert!(
        levels(&r, "hooks:codex-trust").is_empty(),
        "no trust verdict on hooks that can't run"
    );

    // A directory with the wrapper's name, executable bits and all.
    std::fs::remove_file(&wrapper).unwrap();
    std::fs::create_dir(&wrapper).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (_, r) = doctor(&h.ah, &h.home, DEAD);
    assert_eq!(levels(&r, "hooks:codex"), ["fail"], "{r}");
}

/// A hooks.json with one Another hook, its command spelled as given.
fn hooks_with_command(h: &Homes, command: &str) {
    std::fs::create_dir_all(h.home.join(".codex")).unwrap();
    let v = serde_json::json!({ "hooks": { "SessionStart": [
        { "hooks": [{ "type": "command", "command": command }] }
    ] } });
    std::fs::write(h.home.join(".codex/hooks.json"), v.to_string()).unwrap();
}

#[test]
fn wrapper_paths_are_read_quoted_or_bare() {
    let h = homes();
    let missing = h.home.join("gone/another-hook.sh");
    // Bare and with leading spaces, as a hand-edited file may have it.
    for command in [
        format!("{} session-start", missing.display()),
        format!("   '{}' session-start", missing.display()),
    ] {
        hooks_with_command(&h, &command);
        let (ok, r) = doctor(&h.ah, &h.home, DEAD);
        assert!(!ok, "{command}");
        assert!(
            detail(&r, "hooks:codex", "fail").contains(&missing.display().to_string()),
            "{command}: {r}"
        );
    }
}

#[test]
fn codex_trust_is_counted_per_hook_and_needs_a_hash() {
    let h = homes();
    std::fs::create_dir_all(h.home.join(".codex")).unwrap();
    install_hooks(&h, "codex");
    let hooks = h.home.join(".codex/hooks.json").display().to_string();
    let config = h.home.join(".codex/config.toml");
    // What's installed: two or more events, each one group with one hook.
    let hj: Value =
        serde_json::from_str(&std::fs::read_to_string(h.home.join(".codex/hooks.json")).unwrap())
            .unwrap();
    let total = hj["hooks"].as_object().unwrap().len();
    assert!(total >= 2, "{hj}");
    assert!(hj["hooks"].get("SessionStart").is_some(), "{hj}");

    // A partly trusted file: the count, and the untrusted ones named without the path.
    std::fs::write(
        &config,
        format!("[hooks.state.\"{hooks}:session_start:0:0\"]\ntrusted_hash = \"sha256:00\"\n"),
    )
    .unwrap();
    let (_, r) = doctor(&h.ah, &h.home, DEAD);
    let w = find(&r, "hooks:codex-trust", "warn");
    let d = w["detail"].as_str().unwrap();
    assert!(
        d.contains(&format!("trust recorded for 1 of Another's {total}")),
        "{d}"
    );
    let (_, untrusted) = d.split_once("untrusted: ").unwrap();
    assert!(!untrusted.contains("session_start"), "{d}");
    assert!(!untrusted.contains(".codex"), "{d}");
    assert_eq!(untrusted.split(", ").count(), total - 1, "{d}");
    assert!(w["fix"].as_str().unwrap().contains("/hooks"));

    // A table with no hash, a hash under another table, a hash before any table: none count.
    std::fs::write(
        &config,
        format!(
            "trusted_hash = \"sha256:early\"\n\
             [hooks.state.\"{hooks}:session_start:0:0\"]\nenabled = true\n\
             [other]\ntrusted_hash = \"sha256:00\"\n\
             [hooks.state.\"/elsewhere/hooks.json:session_start:0:0\"]\ntrusted_hash = \"sha256:00\"\n"
        ),
    )
    .unwrap();
    let (_, r) = doctor(&h.ah, &h.home, DEAD);
    assert!(
        detail(&r, "hooks:codex-trust", "warn").starts_with("no Codex trust recorded"),
        "{r}"
    );

    // A config.toml that isn't text is the same as having none.
    std::fs::write(&config, [0xff, 0xfe, 0x00]).unwrap();
    let (_, r) = doctor(&h.ah, &h.home, DEAD);
    assert!(detail(&r, "hooks:codex-trust", "warn").starts_with("no Codex trust recorded"));
}

/// BUGS-FOUND.md #4, fixed: `sh_quote` writes an apostrophe as `'\''`, and the wrapper check
/// reads that form back, so a config Another wrote itself gets a real check.
#[test]
fn a_missing_wrapper_under_a_path_with_an_apostrophe_fails() {
    let dir = tempfile::tempdir().unwrap();
    let (ah, home) = (dir.path().join("o'brien"), dir.path().join("home"));
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    std::fs::create_dir_all(&ah).unwrap();
    Store::open(&ah).unwrap();
    let h = Homes {
        _dir: dir,
        ah,
        home,
    };
    install_hooks(&h, "codex");
    std::fs::remove_file(h.ah.join("hooks").join("another-hook.sh")).unwrap();
    let (_, r) = doctor(&h.ah, &h.home, DEAD);
    assert_eq!(levels(&r, "hooks:codex"), ["fail"], "{r}");
    assert!(
        detail(&r, "hooks:codex", "fail").contains("missing or not executable"),
        "{r}"
    );
}

/// BUGS-FOUND.md #4 again, fixed: only a command that runs the wrapper as its first word counts
/// as installed, so this one can't hide a wrapper that is gone.
#[test]
fn a_wrapper_that_is_not_the_first_word_fails() {
    let h = homes();
    let missing = h.home.join("gone/another-hook.sh");
    hooks_with_command(
        &h,
        &format!("/bin/sh '{}' session-start", missing.display()),
    );
    let (_, r) = doctor(&h.ah, &h.home, DEAD);
    assert_eq!(levels(&r, "hooks:codex"), ["fail"], "{r}");
}

/// BUGS-FOUND.md #5, fixed: a command outside `hooks.<event>[].hooks[]` no longer counts as
/// installed, so the trust check never gets to say "all 0 hooks are trusted".
#[test]
fn a_hook_outside_a_group_fails_and_gets_no_trust_verdict() {
    let h = homes();
    std::fs::create_dir_all(h.home.join(".codex")).unwrap();
    install_hooks(&h, "codex");
    let wrapper = h.ah.join("hooks").join("another-hook.sh");
    // The event lists the hook directly, without the group's `hooks` array.
    let v = serde_json::json!({ "hooks": { "SessionStart": [
        { "type": "command", "command": format!("'{}' session-start", wrapper.display()) }
    ] } });
    std::fs::write(h.home.join(".codex/hooks.json"), v.to_string()).unwrap();
    let (ok, r) = doctor(&h.ah, &h.home, DEAD);
    assert!(!ok, "{r}");
    assert_eq!(levels(&r, "hooks:codex"), ["fail"], "{r}");
    assert!(levels(&r, "hooks:codex-trust").is_empty(), "{r}");
}

/// BUGS-FOUND.md #8: `hook install` writes the wrapper path as given, so a relative Another
/// home gives a relative hook command, which won't resolve from a client's working directory.
#[test]
fn bug_hook_install_writes_a_relative_wrapper_for_a_relative_home() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["hook", "install", "--client", "codex"])
        .current_dir(dir.path())
        .env("ANOTHER_HOME", "relative-home")
        .env("HOME", &home)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let config = std::fs::read_to_string(home.join(".codex/hooks.json")).unwrap();
    assert!(
        config.contains("'relative-home/hooks/another-hook.sh' session-start"),
        "today's behaviour; should be an absolute path: {config}"
    );
}

/// BUGS-FOUND.md #6: a hook config that isn't UTF-8 reads as empty, so doctor says "not
/// installed" and offers a fix that refuses to run on that file. PR #25 makes it a failure
/// with a repair fix, with its own tests. What holds on both sides: doctor doesn't pass the
/// file, and the installer refuses it and leaves it alone.
#[test]
fn a_hook_config_that_is_not_utf8_never_passes_and_the_installer_leaves_it_alone() {
    let h = homes();
    std::fs::create_dir_all(h.home.join(".codex")).unwrap();
    let mut bytes = br#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"'/x/another-hook.sh' session-start"}]}]}} "#.to_vec();
    bytes.push(0xff);
    std::fs::write(h.home.join(".codex/hooks.json"), &bytes).unwrap();
    let (_, r) = doctor(&h.ah, &h.home, DEAD);
    assert_ne!(levels(&r, "hooks:codex"), ["pass"], "{r}");
    let out = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["hook", "install", "--client", "codex"])
        .env("ANOTHER_HOME", &h.ah)
        .env("HOME", &h.home)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(
        std::fs::read(h.home.join(".codex/hooks.json")).unwrap(),
        bytes
    );
}
