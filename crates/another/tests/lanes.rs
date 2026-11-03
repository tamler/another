//! Lanes end to end: CLI, the opt-in Stop hook, and remote scoping over authenticated MCP.

use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Command, Stdio};

use another::serve::{http_router, HttpOptions};
use another_core::{Program, RegisterAgent, Store};
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::ServiceExt;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

fn bin(home: &Path, user_home: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_another"));
    c.args(args)
        .env("ANOTHER_HOME", home)
        .env("HOME", user_home)
        .env_remove("ANOTHER_AGENT")
        .env_remove("ANOTHER_LANE")
        .env_remove("GROK_HOOK_EVENT");
    c
}

fn json_of(home: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend_from_slice(args);
    let out = bin(home, home, &all).stdin(Stdio::null()).output().unwrap();
    assert!(
        out.status.success(),
        "another {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn lane_cli_flow_and_markdown_export() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("h");
    let repo = dir.path().join("shop");
    std::fs::create_dir_all(&repo).unwrap();
    let repo_s = repo.to_str().unwrap();
    json_of(
        &home,
        &["agent", "register", "manager", "--program", "grok_bot"],
    );
    json_of(
        &home,
        &["agent", "register", "claude", "--program", "claude"],
    );

    let d = json_of(
        &home,
        &[
            "lane",
            "create",
            "billing",
            "--agent",
            "manager",
            "--project",
            repo_s,
            "--lead",
            "claude",
            "--charter",
            "Add invoices",
            "--status",
            "working",
        ],
    );
    assert_eq!(d["lane"]["status"], "working");
    let r = json_of(
        &home,
        &[
            "lane",
            "report",
            "billing",
            "--agent",
            "claude",
            "--summary",
            "Schema done",
            "--progress",
            "50",
            "--artifact",
            "docs/billing.md",
        ],
    );
    assert_eq!(r["progress"], 50);
    let list = json_of(&home, &["lane", "list"]);
    assert_eq!(list[0]["name"], "billing");
    assert_eq!(list[0]["reports"], 1);
    let inbox = json_of(&home, &["inbox", "--agent", "manager"]);
    assert_eq!(
        inbox["messages"][0]["subject"],
        "[lane billing] working: status report by claude"
    );

    json_of(
        &home,
        &[
            "lane",
            "done",
            "billing",
            "--agent",
            "claude",
            "--outcome",
            "Invoices shipped",
            "--pr",
            "https://github.com/example/repo/pull/3",
        ],
    );
    let out = dir.path().join("lanes");
    let written = json_of(
        &home,
        &["lane", "export", "billing", "--dir", out.to_str().unwrap()],
    );
    assert_eq!(written.as_array().unwrap().len(), 2);
    let status = std::fs::read_to_string(out.join("billing-STATUS.md")).unwrap();
    assert!(status.contains("Status | **done**"));
    assert!(status.contains("pull/3"));
    assert!(out.join("DONE-billing.md").exists());
    let md = bin(&home, &home, &["lane", "show", "billing", "--markdown"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&md.stdout).starts_with("# Lane billing: done"));
    assert!(json_of(&home, &["lane", "list"])
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        json_of(&home, &["lane", "list", "--all"])
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

fn run_stop_hook(home: &Path, user_home: &Path, repo: &Path, message: &str) {
    let mut child = bin(
        home,
        user_home,
        &["hook", "run", "stop", "--client", "codex"],
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .spawn()
    .unwrap();
    let payload = json!({ "cwd": repo, "session_id": "s", "last_assistant_message": message });
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(out.stdout.is_empty(), "stop hook prints nothing");
}

#[test]
fn stop_hook_reports_into_the_lane_with_debounce() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("h");
    let user_home = dir.path().join("u");
    let repo = dir.path().join("shop");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(&user_home).unwrap();

    // Opt-in install adds a Stop hook; a plain install removes it again.
    let ok = |args: &[&str]| {
        assert!(bin(&home, &user_home, args)
            .output()
            .unwrap()
            .status
            .success())
    };
    ok(&["hook", "install", "--client", "codex", "--lane-reports"]);
    let cfg: Value = serde_json::from_str(
        &std::fs::read_to_string(user_home.join(".codex/hooks.json")).unwrap(),
    )
    .unwrap();
    assert!(cfg["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .ends_with("stop --client codex"));
    ok(&["hook", "install", "--client", "codex"]);
    let cfg: Value = serde_json::from_str(
        &std::fs::read_to_string(user_home.join(".codex/hooks.json")).unwrap(),
    )
    .unwrap();
    assert!(cfg["hooks"].get("Stop").is_none());

    // The hook agent is codex@shop; give it a lane it leads.
    let repo_s = repo.to_str().unwrap();
    json_of(
        &home,
        &["agent", "register", "manager", "--program", "grok_bot"],
    );
    json_of(
        &home,
        &[
            "agent",
            "register",
            "codex",
            "--program",
            "codex",
            "--project",
            repo_s,
        ],
    );
    json_of(
        &home,
        &[
            "lane",
            "create",
            "search",
            "--agent",
            "manager",
            "--project",
            repo_s,
            "--lead",
            "codex@shop",
        ],
    );
    run_stop_hook(
        &home,
        &user_home,
        &repo,
        "Indexed the catalogue; next: ranking.",
    );
    run_stop_hook(&home, &user_home, &repo, "second turn, debounced");
    let d = json_of(&home, &["lane", "show", "search"]);
    let reports = d["reports"].as_array().unwrap();
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0]["kind"], "auto");
    assert_eq!(
        reports[0]["summary"],
        "Indexed the catalogue; next: ranking."
    );

    // No lane (or several): nothing happens.
    let other = dir.path().join("other");
    std::fs::create_dir_all(other.join(".git")).unwrap();
    run_stop_hook(&home, &user_home, &other, "no lane here");
}

struct Remote {
    _dir: tempfile::TempDir,
    store: Store,
    url: String,
    ct: CancellationToken,
    _rt: tokio::runtime::Runtime,
}

fn remote() -> Remote {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.ensure_project("t", "/work/shop").unwrap();
    store.ensure_project("t", "/work/vault").unwrap();
    for (n, p) in [("manager", Program::GrokBot), ("claude", Program::Claude)] {
        store
            .register_agent(RegisterAgent {
                name: n.into(),
                program: p,
                project: None,
                model: None,
                description: None,
            })
            .unwrap();
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let ct = CancellationToken::new();
    let app = http_router(
        store.clone(),
        ct.clone(),
        &HttpOptions {
            auth: true,
            ..Default::default()
        },
    );
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let addr = listener.local_addr().unwrap();
    assert_ne!(addr.port(), 7466);
    let stop = ct.clone();
    rt.spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { stop.cancelled_owned().await })
        .await
        .unwrap();
    });
    Remote {
        _dir: dir,
        store,
        url: format!("http://{addr}/mcp"),
        ct,
        _rt: rt,
    }
}

async fn call(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ClientConfig>,
    tool: &str,
    args: Value,
) -> CallToolResult {
    client
        .call_tool(
            CallToolRequestParams::new(tool.to_string())
                .with_arguments(args.as_object().cloned().unwrap()),
        )
        .await
        .unwrap()
}

fn message(r: &CallToolResult) -> String {
    let text = serde_json::to_value(&r.content).unwrap()[0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    serde_json::from_str::<Value>(&text).unwrap()["error"]["message"]
        .as_str()
        .unwrap()
        .to_string()
}

fn code(r: &CallToolResult) -> Option<String> {
    if r.is_error != Some(true) {
        return None;
    }
    let text = serde_json::to_value(&r.content).unwrap()[0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    serde_json::from_str::<Value>(&text).unwrap()["error"]["code"]
        .as_str()
        .map(str::to_string)
}

#[test]
fn remote_token_sees_only_its_projects_lanes() {
    let r = remote();
    for (name, project) in [
        ("shop-lane", "shop"),
        ("vault-lane", "vault"),
        ("api", "shop"),
        ("api", "vault"),
    ] {
        r.store
            .create_lane(
                another_core::NewLane {
                    agent: "manager".into(),
                    name: name.into(),
                    project: project.into(),
                    lead: "claude".into(),
                    charter: format!("secret charter for {project}"),
                    status: None,
                },
                None,
            )
            .unwrap();
    }
    // Vault activity with content the shop token must never read.
    r.store
        .report_lane(
            another_core::LaneReportInput {
                agent: "claude".into(),
                lane: "vault-lane".into(),
                kind: another_core::ReportKind::Status,
                summary: "vault report summary: rotating keys".into(),
                progress: Some(42),
                status: Some(another_core::LaneStatus::Blocked),
                links: vec!["https://example.test/vault-ci".into()],
                artifacts: vec![],
                outcome: None,
                pr_url: None,
            },
            None,
        )
        .unwrap();
    // Locally, a bare name used in two projects is ambiguous; name@project picks one.
    assert_eq!(r.store.lane("api", None).unwrap_err().code(), "ambiguous");
    assert_eq!(
        r.store.lane("api@vault", None).unwrap().lane.project,
        "vault"
    );
    let token = r
        .store
        .create_token("manager", &["/work/shop".into()], None, None)
        .unwrap();
    let url = r.url.clone();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async move {
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(url).auth_header(token.secret),
        );
        let client = ClientConfig::default().serve(transport).await.unwrap();
        let list = call(&client, "lane_list", json!({})).await;
        let lanes = list.structured_content.unwrap()["lanes"].clone();
        let names: Vec<&str> = lanes
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["api", "shop-lane"]);
        assert!(!lanes.to_string().contains("vault"));

        // Out-of-scope lanes look exactly like lanes that don't exist.
        let hidden = call(&client, "lane_show", json!({ "lane": "vault-lane" })).await;
        let missing = call(&client, "lane_show", json!({ "lane": "nope-lane" })).await;
        assert_eq!(code(&hidden).as_deref(), Some("not_found"));
        assert_eq!(
            message(&hidden).replace("vault-lane", "X"),
            message(&missing).replace("nope-lane", "X")
        );
        // The bare name `api` resolves to the shop lane for this token (vault's is invisible).
        let api = call(&client, "lane_show", json!({ "lane": "api" })).await;
        assert_eq!(api.structured_content.unwrap()["lane"]["project"], "shop");
        assert_eq!(
            code(&call(&client, "lane_show", json!({ "lane": "api@vault" })).await).as_deref(),
            Some("not_found")
        );

        // No lane content anywhere in the feed or summary, and no vault lane events. (The
        // node-wide parts of the feed, D16, are scoped by the push phase.)
        for tool in ["activity_feed", "activity_summarize"] {
            let feed = call(&client, tool, json!({ "since": "1h" })).await;
            let text = serde_json::to_string(&feed.content).unwrap();
            for leak in ["secret charter", "rotating keys", "vault-ci", "42%", "(42"] {
                assert!(!text.contains(leak), "{tool} leaked {leak}: {text}");
            }
            assert!(text.contains("shop-lane"), "{tool}: {text}");
        }
        let feed = call(
            &client,
            "activity_feed",
            json!({ "since": "1h", "kinds": ["lane"] }),
        )
        .await;
        let items = feed.structured_content.unwrap()["items"].clone();
        assert!(!items.as_array().unwrap().is_empty());
        assert!(!items.to_string().contains("vault"), "{items}");
        assert_eq!(
            code(&call(&client, "lane_show", json!({ "lane": "shop-lane" })).await),
            None
        );
        let sneak = call(
            &client,
            "lane_create",
            json!({ "name": "x", "project": "/work/vault", "lead": "claude" }),
        )
        .await;
        assert_eq!(code(&sneak).as_deref(), Some("forbidden"));
        let report = call(
            &client,
            "lane_report",
            json!({ "lane": "vault-lane", "summary": "peek" }),
        )
        .await;
        assert_eq!(code(&report).as_deref(), Some("not_found"));
        let ok = call(
            &client,
            "lane_report",
            json!({ "lane": "shop-lane", "summary": "from the manager", "status": "working" }),
        )
        .await;
        assert_eq!(code(&ok), None, "{ok:?}");
        let spoof = call(
            &client,
            "lane_close",
            json!({ "agent": "claude", "lane": "shop-lane" }),
        )
        .await;
        assert_eq!(code(&spoof).as_deref(), Some("forbidden"));
        client.cancel().await.unwrap();
    });
    let audit = r.store.audit_log(100).unwrap();
    assert!(audit
        .iter()
        .any(|a| a.operation == "tools/call lane_list" && a.outcome == "ok"));
    assert!(audit
        .iter()
        .any(|a| a.operation == "tools/call lane_create" && a.outcome == "denied"));
    assert!(audit
        .iter()
        .any(|a| a.operation == "tools/call lane_show" && a.outcome == "error"));
    r.ct.cancel();
}
