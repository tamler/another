//! Authenticated remote access: bearer tokens, scoping, audit log, GET /activity.

use std::net::SocketAddr;
use std::path::Path;

use another::serve::{http_router, HttpOptions};
use another_core::{
    NewRequest, Program, RegisterAgent, RequestAction, RequestFilter, RequestUpdate, SendMessage,
    Store,
};
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

type Client = RunningService<RoleClient, ClientConfig>;

struct Server {
    _dir: tempfile::TempDir,
    store: Store,
    base: String,
    ct: CancellationToken,
    work: String,
    secret: String,
    work_glob: String,
}

async fn start() -> Server {
    start_with_federation(false).await
}

async fn start_with_federation(federation: bool) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap().unguarded_repair();
    let work = dir.path().join("work/shop").to_string_lossy().into_owned();
    let secret = dir
        .path()
        .join("secret/vault")
        .to_string_lossy()
        .into_owned();
    let work_glob = dir
        .path()
        .join("work/*")
        .to_string_lossy()
        .replace('\\', "/");
    store.ensure_project("t", &work).unwrap();
    store.ensure_project("t", &secret).unwrap();
    for (name, program, project) in [
        ("grok-bot", Program::GrokBot, None),
        ("alice", Program::Claude, Some(work.as_str())),
    ] {
        store
            .register_agent(RegisterAgent {
                name: name.into(),
                program,
                project: project.map(Into::into),
                model: None,
                description: None,
            })
            .unwrap();
    }
    let ct = CancellationToken::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let opts = HttpOptions {
        legacy_repair: true,
        auth: true,
        federation,
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
        work,
        secret,
        work_glob,
        ct,
    }
}

async fn connect(base: &str, token: &str) -> Client {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("{base}/mcp")).auth_header(token),
    );
    ClientConfig::default().serve(transport).await.unwrap()
}

async fn call(client: &Client, tool: &str, args: Value) -> CallToolResult {
    client
        .call_tool(
            CallToolRequestParams::new(tool.to_string())
                .with_arguments(args.as_object().cloned().unwrap()),
        )
        .await
        .unwrap()
}

fn error_code(res: &CallToolResult) -> Option<String> {
    if res.is_error != Some(true) {
        return None;
    }
    let text = serde_json::to_value(&res.content).unwrap()[0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    let v: Value = serde_json::from_str(&text).unwrap();
    v["error"]["code"].as_str().map(str::to_string)
}

#[tokio::test]
async fn federated_listener_rejects_conflicting_mcp_heartbeat() {
    let srv = start_with_federation(true).await;
    let token = srv
        .store
        .create_token("grok-bot", std::slice::from_ref(&srv.work_glob), None, None)
        .unwrap();
    let client = connect(&srv.base, &token.secret).await;
    let before = srv.store.agent("grok-bot").unwrap();
    let conflict = call(&client, "heartbeat", json!({ "start": true, "end": true })).await;
    assert_eq!(error_code(&conflict).as_deref(), Some("invalid"));
    let after = srv.store.agent("grok-bot").unwrap();
    assert_eq!(after.status, before.status);
    assert_eq!(after.last_seen_at, before.last_seen_at);
    client.cancel().await.unwrap();
    srv.ct.cancel();
}

#[tokio::test]
async fn bearer_tokens_scope_and_audit_remote_calls() {
    let srv = start().await;
    let http = reqwest::Client::new();

    // No or bad token: 401 on /mcp and /activity; /health stays open.
    let r = http
        .get(format!("{}/activity", srv.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = http
        .post(format!("{}/mcp", srv.base))
        .header("authorization", "Bearer ano_nope_nope")
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
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
    assert_eq!(health["auth"], true);

    let token = srv
        .store
        .create_token(
            "grok-bot",
            std::slice::from_ref(&srv.work_glob),
            Some("bot".into()),
            None,
        )
        .unwrap();
    let client = connect(&srv.base, &token.secret).await;

    let before = srv.store.agent("grok-bot").unwrap();
    let conflict = call(&client, "heartbeat", json!({ "start": true, "end": true })).await;
    assert_eq!(error_code(&conflict).as_deref(), Some("invalid"));
    let after = srv.store.agent("grok-bot").unwrap();
    assert_eq!(after.status, before.status);
    assert_eq!(after.last_seen_at, before.last_seen_at);

    // Acting agent is filled in, and only the token's agent is allowed.
    let inbox = call(&client, "fetch_inbox", json!({})).await;
    assert_eq!(error_code(&inbox), None);
    assert_eq!(inbox.structured_content.unwrap()["agent"], "grok-bot");
    let other = call(&client, "fetch_inbox", json!({ "agent": "alice" })).await;
    assert_eq!(error_code(&other).as_deref(), Some("forbidden"));

    let sent = call(
        &client,
        "send_message",
        json!({ "to": ["alice"], "subject": "status?", "body": "TOP SECRET BODY" }),
    )
    .await;
    assert_eq!(error_code(&sent), None);
    let spoof = call(
        &client,
        "send_message",
        json!({ "from": "alice", "to": ["grok-bot"], "subject": "x", "body": "y" }),
    )
    .await;
    assert_eq!(error_code(&spoof).as_deref(), Some("forbidden"));

    // Project globs.
    let ok = call(
        &client,
        "hold_paths",
        json!({ "project": srv.work, "paths": ["docs/**"] }),
    )
    .await;
    assert_eq!(error_code(&ok), None);
    for args in [
        json!({ "project": srv.secret, "paths": ["a"] }),
        json!({ "project": "vault", "paths": ["a"] }),
        json!({ "action": "list" }),
    ] {
        let denied = call(&client, "hold_paths", args).await;
        assert_eq!(error_code(&denied).as_deref(), Some("forbidden"));
    }
    let denied = call(
        &client,
        "ensure_project",
        json!({ "path": srv._dir.path().join("elsewhere/x") }),
    )
    .await;
    assert_eq!(error_code(&denied).as_deref(), Some("forbidden"));

    // Never reap remotely; orphan listing never scans processes.
    let reap = call(&client, "resource_reap", json!({ "target": "pid:1" })).await;
    assert_eq!(error_code(&reap).as_deref(), Some("forbidden"));
    let orphans = call(&client, "orphans_list", json!({ "scan": true })).await;
    assert!(orphans.structured_content.unwrap()["scan"].is_null());

    // Threads: only ones the token's agent is part of.
    let private = srv
        .store
        .send_message(SendMessage {
            from: "alice".into(),
            to: vec!["alice".into()],
            subject: "note to self".into(),
            body: "private".into(),
            ..Default::default()
        })
        .unwrap();
    let t = call(
        &client,
        "threads",
        json!({ "thread_id": private.thread_id }),
    )
    .await;
    assert_eq!(error_code(&t).as_deref(), Some("forbidden"));

    // Feed reads are allowed, over MCP and HTTP.
    let feed = call(&client, "activity_feed", json!({ "kinds": ["mail"] })).await;
    assert_eq!(error_code(&feed), None);
    let r = http
        .get(format!("{}/activity?since=1h&kinds=mail,hold", srv.base))
        .header("authorization", format!("Bearer {}", token.secret))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = serde_json::from_str(&r.text().await.unwrap()).unwrap();
    assert!(!body["items"].as_array().unwrap().is_empty());
    assert!(!body.to_string().contains("TOP SECRET BODY"));

    // Audit log: every remote call, denials included, without message bodies.
    let audit = srv.store.audit_log(100).unwrap();
    let ops: Vec<(&str, &str)> = audit
        .iter()
        .map(|e| (e.operation.as_str(), e.outcome.as_str()))
        .collect();
    assert!(ops.contains(&("GET /activity", "denied")));
    assert!(ops.contains(&("POST /mcp", "denied")));
    assert!(ops.contains(&("tools/call send_message", "ok")));
    assert!(ops.contains(&("tools/call send_message", "denied")));
    assert!(ops.contains(&("tools/call resource_reap", "denied")));
    assert!(ops.contains(&("GET /activity", "ok")));
    assert!(audit
        .iter()
        .filter(|e| e.token_id.is_some())
        .all(|e| e.agent.as_deref() == Some("grok-bot")));
    let all = serde_json::to_string(&audit).unwrap();
    assert!(!all.contains("TOP SECRET BODY"), "bodies are redacted");
    assert!(all.contains("<15 bytes>"));

    // Revocation takes effect on the next request.
    srv.store.revoke_token(&token.token.token_id).unwrap();
    let r = http
        .get(format!("{}/activity", srv.base))
        .header("authorization", format!("Bearer {}", token.secret))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let after = client
        .call_tool(CallToolRequestParams::new("fetch_inbox").with_arguments(Default::default()))
        .await;
    assert!(after.is_err() || after.unwrap().is_error == Some(true));
    srv.ct.cancel();
}

#[tokio::test]
async fn expired_and_unscoped_tokens() {
    let srv = start().await;
    // No project globs: project-scoped calls are denied, mailbox calls work.
    let token = srv.store.create_token("grok-bot", &[], None, None).unwrap();
    let client = connect(&srv.base, &token.secret).await;
    let hold = call(
        &client,
        "hold_paths",
        json!({ "project": srv.work, "paths": ["a"] }),
    )
    .await;
    assert_eq!(error_code(&hold).as_deref(), Some("forbidden"));
    let hb = call(&client, "heartbeat", json!({})).await;
    assert_eq!(error_code(&hb), None);

    // Registering itself is fine; registering someone else is not.
    let me = call(
        &client,
        "register_agent",
        json!({ "name": "grok-bot", "program": "grok_bot" }),
    )
    .await;
    assert_eq!(error_code(&me), None);
    let other = call(
        &client,
        "register_agent",
        json!({ "name": "mallory", "program": "other" }),
    )
    .await;
    assert_eq!(error_code(&other).as_deref(), Some("forbidden"));

    assert!(srv
        .store
        .create_token("grok-bot", &["[".into()], None, None)
        .is_err());
    assert!(srv.store.create_token("nobody", &[], None, None).is_err());
    let short = srv
        .store
        .create_token("grok-bot", &[], None, Some(1))
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    assert!(srv.store.verify_token(&short.secret).is_err());
    assert!(srv.store.verify_token("not-a-token").is_err());
    let tampered = format!("{}x", token.secret);
    assert!(srv.store.verify_token(&tampered).is_err());
    srv.ct.cancel();
}

/// A remote peer reads the work board for one project inside its globs, never node-wide.
#[tokio::test]
async fn work_board_is_one_project_inside_the_globs() {
    let srv = start().await;
    let token = srv
        .store
        .create_token("grok-bot", std::slice::from_ref(&srv.work_glob), None, None)
        .unwrap();
    let client = connect(&srv.base, &token.secret).await;
    let all = call(&client, "work_board", json!({})).await;
    assert_eq!(error_code(&all).as_deref(), Some("forbidden"));
    let secret = call(&client, "work_board", json!({ "project": srv.secret })).await;
    assert_eq!(error_code(&secret).as_deref(), Some("forbidden"));
    let shop = call(&client, "work_board", json!({ "project": srv.work })).await;
    assert_eq!(error_code(&shop), None);
    let board = shop.structured_content.unwrap();
    let slugs: Vec<&str> = board["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["project"].as_str().unwrap())
        .collect();
    assert_eq!(slugs, ["shop"]);
    assert_eq!(board["confined"], true);
    srv.ct.cancel();
}

/// Review round 1, P1: the holder of a stopped edit in a project outside the token's globs
/// is not named, though the stopped edit itself is in view. Since D92 a hold blocks only
/// inside its checkout, so the holder's project is the top of the checkout that
/// `work/shop` is a subdirectory project of.
#[tokio::test]
async fn work_board_hides_a_holder_outside_the_globs() {
    let srv = start().await;
    let top = srv._dir.path();
    std::fs::create_dir_all(top.join(".git")).unwrap();
    std::fs::write(
        top.join(".git/config"),
        "[remote \"origin\"]\nurl = https://example.com/team/shop.git\n",
    )
    .unwrap();
    // `start` registered shop before it was inside a git checkout; ensure it again so its
    // link is recorded and the hold taken through the top-level project conflicts with it.
    srv.store.ensure_project("t", &srv.work).unwrap();
    let holder = srv
        .store
        .register_agent(RegisterAgent {
            name: "private-holder".into(),
            program: Program::Claude,
            project: Some(top.to_string_lossy().into_owned()),
            model: None,
            description: None,
        })
        .unwrap();
    let file = Path::new(&srv.work)
        .join("src/internal.rs")
        .to_string_lossy()
        .into_owned();
    let held = srv
        .store
        .guard_edit(
            &holder.address,
            top.to_str().unwrap(),
            std::slice::from_ref(&file),
            another_core::EDIT_HOOK_HOLD_REASON,
        )
        .unwrap();
    assert!(held.conflicts.is_empty());
    let stopped = srv
        .store
        .guard_edit(
            "alice@shop",
            &srv.work,
            std::slice::from_ref(&file),
            another_core::EDIT_HOOK_HOLD_REASON,
        )
        .unwrap();
    assert_eq!(
        stopped.conflicts.len(),
        1,
        "the hold in the top-level project must stop alice"
    );

    let token = srv
        .store
        .create_token("grok-bot", std::slice::from_ref(&srv.work_glob), None, None)
        .unwrap();
    let client = connect(&srv.base, &token.secret).await;
    let res = call(&client, "work_board", json!({ "project": srv.work })).await;
    assert_eq!(error_code(&res), None);
    let board = res.structured_content.unwrap();
    let blocked = &board["projects"][0]["blocked"][0];
    assert_eq!(blocked["agent"], "alice@shop");
    assert_eq!(blocked["path"], "src/internal.rs");
    assert!(blocked["holder"].is_null() && blocked["holder_project"].is_null());
    assert!(blocked["pattern"].is_null());
    let text = board.to_string();
    assert!(!text.contains("private-holder"), "{text}");
    assert!(!text.contains("work/shop/src/internal.rs"), "{text}");
    // Nor is the holder's project named, as a project, in an address or in a hold key: it
    // lies outside the token's globs. The slug is looked for as a whole value, since the
    // board's own project path runs through that directory.
    let slug = srv.store.project(top.to_str().unwrap()).unwrap().slug;
    assert_eq!(holder.address, format!("private-holder@{slug}"));
    for named in [
        format!("\"{slug}\""),
        format!("@{slug}"),
        format!("\"{slug}:"),
    ] {
        assert!(!text.contains(&named), "{named} in {text}");
    }
    client.cancel().await.unwrap();
    srv.ct.cancel();
}

/// Review round 1, P1: request participants registered outside the token's globs are not
/// named, in either direction, and neither is an outside owner.
#[tokio::test]
async fn work_board_hides_request_participants_outside_the_globs() {
    let srv = start().await;
    srv.store
        .register_agent(RegisterAgent {
            name: "vault-owner".into(),
            program: Program::Claude,
            project: Some(srv.secret.clone()),
            model: None,
            description: None,
        })
        .unwrap();
    let request = |from: &str, to: &[&str], subject: &str| {
        srv.store
            .send_message(SendMessage {
                from: from.into(),
                to: to.iter().map(|s| s.to_string()).collect(),
                subject: subject.into(),
                body: "fixture".into(),
                request: Some(NewRequest::default()),
                ..Default::default()
            })
            .unwrap()
    };
    request("alice@shop", &["vault-owner@vault"], "Outbound");
    request("vault-owner@vault", &["alice@shop"], "Inbound");
    request("alice@shop", &["alice@shop"], "Owned outside");
    let owned = srv
        .store
        .requests(RequestFilter::default())
        .unwrap()
        .into_iter()
        .find(|r| r.title == "Owned outside")
        .unwrap();
    srv.store
        .update_request(RequestUpdate {
            agent: "vault-owner@vault".into(),
            request_id: owned.request_id,
            action: RequestAction::Claim,
            note: None,
            due_in_seconds: None,
        })
        .unwrap();

    let token = srv
        .store
        .create_token("grok-bot", std::slice::from_ref(&srv.work_glob), None, None)
        .unwrap();
    let client = connect(&srv.base, &token.secret).await;
    let res = call(&client, "work_board", json!({ "project": "shop" })).await;
    assert_eq!(error_code(&res), None);
    let board = res.structured_content.unwrap();
    let text = board.to_string();
    assert!(
        !text.contains("vault-owner"),
        "outside participant leaked: {text}"
    );
    assert!(!text.contains("vault"), "outside project leaked: {text}");
    let by_title = |t: &str| {
        board["requests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["title"] == t)
            .unwrap()
            .clone()
    };
    let out = by_title("Outbound");
    assert_eq!(out["from"], "alice@shop");
    assert_eq!(
        (out["to"].clone(), out["to_hidden"].clone()),
        (json!([]), json!(1))
    );
    let inbound = by_title("Inbound");
    assert!(inbound["from"].is_null());
    assert_eq!(inbound["to"], json!(["alice@shop"]));
    let owned = by_title("Owned outside");
    assert!(owned["owner"].is_null());
    assert_eq!(owned["owner_hidden"], true);
    client.cancel().await.unwrap();
    srv.ct.cancel();
}

#[test]
fn token_cli_and_bind_rules() {
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        {
            let mut legacy_fixture_command =
                std::process::Command::new(env!("CARGO_BIN_EXE_another"));
            legacy_fixture_command.arg("--unguarded-repair");
            legacy_fixture_command
        }
        .args(args)
        .env("ANOTHER_HOME", dir.path())
        .output()
        .unwrap()
    };
    assert!(
        run(&["agent", "register", "grok-bot", "--program", "grok_bot"])
            .status
            .success()
    );
    let out = run(&[
        "--json",
        "token",
        "create",
        "--agent",
        "grok-bot",
        "--project",
        "/work/*",
        "--expires",
        "30d",
    ]);
    assert!(out.status.success());
    let created: Value = serde_json::from_slice(&out.stdout).unwrap();
    let secret = created["secret"].as_str().unwrap();
    assert!(secret.starts_with("ano_"));
    let list = run(&["--json", "token", "list"]);
    let list_text = String::from_utf8_lossy(&list.stdout).to_string();
    assert!(!list_text.contains(secret), "secrets are never listed");
    let listed: Value = serde_json::from_str(&list_text).unwrap();
    assert_eq!(listed[0]["agent"], "grok-bot");
    assert!(listed[0]["expires_at"].is_string());
    let id = created["token_id"].as_str().unwrap();
    assert!(run(&["token", "revoke", id]).status.success());
    let listed: Value = serde_json::from_slice(&run(&["--json", "token", "list"]).stdout).unwrap();
    assert!(listed[0]["revoked_at"].is_string());

    // Non-loopback binds need --auth and --allow-remote-bind.
    let no_auth = run(&["serve", "--bind", "0.0.0.0:0"]);
    assert!(!no_auth.status.success());
    let auth_only = run(&["serve", "--auth", "--bind", "0.0.0.0:0"]);
    assert!(!auth_only.status.success());
    assert!(String::from_utf8_lossy(&auth_only.stderr).contains("--allow-remote-bind"));
    let flag_only = run(&["serve", "--allow-remote-bind", "--bind", "0.0.0.0:0"]);
    assert!(!flag_only.status.success(), "clap requires --auth");
}
