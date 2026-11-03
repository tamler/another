//! Spend end to end: the CLI flow (accounts, record, borrow with approval, burn-down), and
//! remote access over authenticated MCP and `GET /spend` (scoped, audited, accounts local
//! only). Temporary ANOTHER_HOME and ephemeral ports only.

use std::net::SocketAddr;
use std::path::Path;
use std::process::{Command, Stdio};

use another::serve::{http_router, HttpOptions};
use another_core::{NewSpendAccount, Program, RegisterAgent, SpendKind, SpendPeriod, Store};
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

fn bin(home: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_another"));
    c.args(args)
        .env("ANOTHER_HOME", home)
        .env("HOME", home)
        .env_remove("ANOTHER_AGENT")
        .env_remove("ANOTHER_DECIDER")
        .stdin(Stdio::null());
    c
}

fn ok(home: &Path, args: &[&str]) -> String {
    let out = bin(home, args).output().unwrap();
    assert!(
        out.status.success(),
        "another {args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn spend_cli_flow() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let repo = home.join("shop");
    std::fs::create_dir_all(&repo).unwrap();
    let repo_s = repo.to_str().unwrap();
    ok(
        home,
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

    // The documented seed examples (docs only; nothing is hard-coded).
    ok(
        home,
        &[
            "spend",
            "account",
            "add",
            "claude-api",
            "--kind",
            "api_credits",
            "--unit",
            "USD",
            "--budget",
            "200",
        ],
    );
    ok(
        home,
        &[
            "spend",
            "account",
            "add",
            "codex-resets",
            "--kind",
            "plan_reset",
            "--unit",
            "resets",
            "--budget",
            "2",
            "--reset-day",
            "15",
        ],
    );
    ok(
        home,
        &[
            "spend",
            "account",
            "add",
            "gpu-a100",
            "--kind",
            "gpu_pod",
            "--unit",
            "USD",
            "--budget",
            "150",
            "--cost-per-hour",
            "2.10",
            "--requires-approval",
            "--approver",
            "sam",
            "-p",
            repo_s,
        ],
    );
    let list = ok(home, &["spend", "account", "list"]);
    assert!(
        list.contains("claude-api")
            && list.contains("200 USD/month from day 1")
            && list.contains("[shared]"),
        "{list}"
    );
    assert!(
        list.contains("approval by sam") && list.contains("[shop]"),
        "{list}"
    );
    let bad = bin(
        home,
        &[
            "spend", "account", "add", "x", "--kind", "yacht", "--unit", "USD",
        ],
    )
    .output()
    .unwrap();
    assert!(!bad.status.success());

    ok(
        home,
        &[
            "spend",
            "record",
            "--agent",
            "codex@shop",
            "--account",
            "claude-api",
            "--amount",
            "12.5",
            "--note",
            "batch eval",
        ],
    );
    ok(
        home,
        &[
            "spend",
            "record",
            "--agent",
            "codex@shop",
            "--account",
            "codex-resets",
            "--amount",
            "1",
        ],
    );

    // Borrowing the GPU asks sam; answering approve starts the lease.
    let out = ok(
        home,
        &[
            "--json",
            "spend",
            "borrow",
            "--agent",
            "codex@shop",
            "--account",
            "gpu-a100",
            "--hours",
            "3",
            "--note",
            "fine-tune",
        ],
    );
    let o: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(o["lease"]["status"], "pending_approval");
    let decision = o["decision_id"].as_str().unwrap().to_string();
    let inbox = ok(home, &["inbox", "--agent", "sam"]);
    assert!(
        inbox.contains("Approve codex@shop borrowing gpu-a100"),
        "{inbox}"
    );
    ok(home, &["decision", "answer", &decision, "approve"]);
    let leases = ok(home, &["spend", "leases"]);
    assert!(
        leases.contains("active") && leases.contains("gpu-a100 by codex@shop"),
        "{leases}"
    );
    let lease_id = o["lease"]["lease_id"].as_str().unwrap();
    ok(
        home,
        &["spend", "release", lease_id, "--agent", "codex@shop"],
    );

    let md = ok(home, &["spend"]);
    assert!(md.starts_with("**Spend**"), "{md}");
    assert!(md.contains("`claude-api` 12.50 / 200 USD"), "{md}");
    assert!(md.contains("`codex-resets` 1 / 2 resets  50%"), "{md}");
    assert!(md.contains("Top agents: codex@shop"), "{md}");
    let j: Value =
        serde_json::from_str(&ok(home, &["--json", "spend", "--account", "claude-api"])).unwrap();
    assert_eq!(j["accounts"].as_array().unwrap().len(), 1);
    assert_eq!(j["accounts"][0]["spent"], 12.5);
    let prev: Value =
        serde_json::from_str(&ok(home, &["--json", "spend", "--period", "previous"])).unwrap();
    assert_eq!(prev["accounts"][0]["spent"], 0.0);
}

type Client = RunningService<RoleClient, ClientConfig>;

async fn call(client: &Client, tool: &str, args: Value) -> CallToolResult {
    client
        .call_tool(
            CallToolRequestParams::new(tool.to_string())
                .with_arguments(args.as_object().cloned().unwrap()),
        )
        .await
        .unwrap()
}

fn body(r: &CallToolResult) -> Value {
    let t = serde_json::to_value(&r.content).unwrap()[0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    serde_json::from_str(&t).unwrap()
}

fn code(r: &CallToolResult) -> Option<String> {
    (r.is_error == Some(true))
        .then(|| body(r)["error"]["code"].as_str().map(str::to_string))
        .flatten()
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_spend_is_scoped_and_audited() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.ensure_project("t", "/work/shop").unwrap();
    store.ensure_project("t", "/work/vault").unwrap();
    for (name, project) in [
        ("manager", None),
        ("codex", Some("/work/shop")),
        ("vaulter", Some("/work/vault")),
    ] {
        store
            .register_agent(RegisterAgent {
                name: name.into(),
                program: Program::Codex,
                project: project.map(Into::into),
                model: None,
                description: None,
            })
            .unwrap();
    }
    let acct = |name: &str, project: Option<&str>| {
        store
            .create_spend_account(
                NewSpendAccount {
                    name: name.into(),
                    kind: SpendKind::ApiCredits,
                    unit: "USD".into(),
                    budget: Some(100.0),
                    period: SpendPeriod::Monthly,
                    reset_day: 1,
                    cost_per_hour: None,
                    requires_approval: false,
                    approver: None,
                    project: project.map(Into::into),
                    description: None,
                },
                None,
            )
            .unwrap()
    };
    acct("shared-api", None);
    acct("vault-only", Some("vault"));
    for (agent, account, amount) in [
        ("codex@shop", "shared-api", 10.0),
        ("vaulter@vault", "shared-api", 55.0),
        ("vaulter@vault", "vault-only", 7.0),
    ] {
        store
            .record_spend(
                another_core::RecordSpend {
                    agent: Some(agent.into()),
                    account: account.into(),
                    amount,
                    ..Default::default()
                },
                None,
            )
            .unwrap();
    }
    let token = store
        .create_token("manager", &["/work/shop".into()], None, None)
        .unwrap()
        .secret;
    let ct = CancellationToken::new();
    let app = http_router(
        store.clone(),
        ct.clone(),
        &HttpOptions {
            auth: true,
            ..Default::default()
        },
    );
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
    let base = format!("http://{addr}");
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("{base}/mcp"))
            .auth_header(token.clone()),
    );
    let client: Client = ClientConfig::default().serve(transport).await.unwrap();

    let r = body(&call(&client, "spend_report", json!({})).await);
    let names: Vec<&str> = r["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["account"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["shared-api"]);
    assert_eq!(r["accounts"][0]["spent"], 10.0);
    assert_eq!(r["accounts"][0]["partial"], true);
    assert!(r["accounts"][0]["budget"].is_null());
    assert!(!r.to_string().contains("vault"), "{r}");

    let r = call(
        &client,
        "spend_account_upsert",
        json!({ "name": "x", "kind": "server", "unit": "USD" }),
    )
    .await;
    assert_eq!(code(&r).as_deref(), Some("forbidden"));
    let r = call(
        &client,
        "spend_record",
        json!({ "account": "vault-only", "amount": 1 }),
    )
    .await;
    assert_eq!(code(&r).as_deref(), Some("forbidden"));
    let r = call(
        &client,
        "spend_record",
        json!({ "agent": "vaulter@vault", "account": "shared-api", "amount": 1 }),
    )
    .await;
    assert_eq!(
        code(&r).as_deref(),
        Some("forbidden"),
        "acting as another agent"
    );
    let r = call(
        &client,
        "spend_record",
        json!({ "account": "shared-api", "amount": 2.5, "project": "shop" }),
    )
    .await;
    assert_eq!(code(&r), None, "{}", body(&r));

    let http = reqwest::Client::new();
    assert_eq!(
        http.get(format!("{base}/spend"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let resp = http
        .get(format!("{base}/spend"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let j: Value = resp.json().await.unwrap();
    assert_eq!(j["accounts"][0]["spent"], 12.5);
    assert!(!j.to_string().contains("vault"));

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let ops: Vec<(String, String)> = store
        .audit_log(100)
        .unwrap()
        .into_iter()
        .map(|a| (a.operation, a.outcome))
        .collect();
    for (op, outcome) in [
        ("GET /spend", "ok"),
        ("GET /spend", "denied"),
        ("tools/call spend_report", "ok"),
        ("tools/call spend_account_upsert", "denied"),
        ("tools/call spend_record", "error"),
        ("tools/call spend_record", "denied"),
    ] {
        assert!(
            ops.contains(&(op.to_string(), outcome.to_string())),
            "missing {op} {outcome}: {ops:?}"
        );
    }
    client.cancel().await.unwrap();
    ct.cancel();
}
