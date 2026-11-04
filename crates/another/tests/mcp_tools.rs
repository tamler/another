//! The spend, lane, decision and resource MCP tools through real rmcp clients: in-process as a
//! local caller, and over HTTP with bearer tokens, where each token acts as one agent and sees
//! only its projects (docs/REMOTE.md).

use std::net::SocketAddr;

use another::mcp::AnotherMcp;
use another::serve::{http_router, HttpOptions};
use another_core::*;
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

type Client = RunningService<RoleClient, ClientConfig>;

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    base: String,
    stop: CancellationToken,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.ensure_project("t", "/work/shop").unwrap();
    store.ensure_project("t", "/work/vault").unwrap();
    for (name, program, project) in [
        ("manager", Program::GrokBot, None),
        ("claude", Program::Claude, Some("/work/shop")),
        ("codex", Program::Codex, Some("/work/shop")),
        ("vaulter", Program::Claude, Some("/work/vault")),
    ] {
        store
            .register_agent_trusted(RegisterAgent {
                name: name.into(),
                program,
                project: project.map(Into::into),
                model: None,
                description: None,
            })
            .unwrap();
    }
    let stop = CancellationToken::new();
    let app = http_router(
        store.clone(),
        stop.clone(),
        &HttpOptions {
            auth: true,
            ..Default::default()
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    assert_ne!(addr.port(), 7466);
    let cancelled = stop.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { cancelled.cancelled_owned().await })
        .await
        .unwrap();
    });
    Fixture {
        _dir: dir,
        store,
        base: format!("http://{addr}"),
        stop,
    }
}

impl Fixture {
    async fn local(&self) -> Client {
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        let store = self.store.clone();
        tokio::spawn(async move {
            let server = AnotherMcp::new(store).serve(server_io).await.unwrap();
            let _ = server.waiting().await;
        });
        ClientConfig::default().serve(client_io).await.unwrap()
    }

    /// A client whose token acts as `agent` within `glob`.
    async fn remote(&self, agent: &str, glob: &str) -> Client {
        let token = self
            .store
            .create_token(agent, &[glob.to_string()], None, None)
            .unwrap()
            .secret;
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(format!("{}/mcp", self.base))
                .auth_header(token),
        );
        ClientConfig::default().serve(transport).await.unwrap()
    }
}

async fn call(client: &Client, tool: &str, args: Value) -> CallToolResult {
    client
        .call_tool(
            CallToolRequestParams::new(tool.to_string())
                .with_arguments(args.as_object().cloned().expect("object args")),
        )
        .await
        .expect("tool call")
}

async fn ok(client: &Client, tool: &str, args: Value) -> Value {
    let res = call(client, tool, args).await;
    assert_ne!(res.is_error, Some(true), "{tool} failed: {res:?}");
    res.structured_content.expect("structured output")
}

/// The error's `(code, message)`.
async fn err(client: &Client, tool: &str, args: Value) -> (String, String) {
    let res = call(client, tool, args).await;
    assert_eq!(res.is_error, Some(true), "{tool} should fail: {res:?}");
    let text = serde_json::to_value(&res.content).unwrap()[0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    let body: Value = serde_json::from_str(&text).unwrap();
    (
        body["error"]["code"].as_str().unwrap().to_string(),
        body["error"]["message"].as_str().unwrap().to_string(),
    )
}

fn names(v: &Value, key: &str, field: &str) -> Vec<String> {
    v[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x[field].as_str().unwrap().to_string())
        .collect()
}

fn account(store: &Store, name: &str, project: Option<&str>) {
    store
        .create_spend_account(
            NewSpendAccount {
                name: name.into(),
                kind: SpendKind::GpuPod,
                unit: "USD".into(),
                budget: Some(100.0),
                period: SpendPeriod::Monthly,
                reset_day: 1,
                cost_per_hour: Some(2.0),
                requires_approval: false,
                approver: None,
                project: project.map(Into::into),
                description: None,
            },
            None,
        )
        .unwrap();
}

// ---------------------------------------------------------------------------------------
// Spend

#[tokio::test(flavor = "multi_thread")]
async fn the_spend_tools_cover_an_accounts_life() {
    let f = fixture().await;
    let c = f.local().await;

    // Creating needs a kind and a unit.
    let (code, msg) = err(&c, "spend_account_upsert", json!({ "name": "gpu" })).await;
    assert_eq!(code, "invalid");
    assert!(msg.contains("kind is required"), "{msg}");
    let (_, msg) = err(
        &c,
        "spend_account_upsert",
        json!({ "name": "gpu", "kind": "gpu_pod" }),
    )
    .await;
    assert!(msg.contains("unit is required"), "{msg}");

    let made = ok(
        &c,
        "spend_account_upsert",
        json!({ "name": " GPU ", "kind": "gpu_pod", "unit": "USD", "budget": 100.0, "cost_per_hour": 2.0, "reset_day": 15, "project": "shop", "description": "pod" }),
    )
    .await;
    assert_eq!(made["name"], "gpu");
    assert_eq!(made["reset_day"], 15);
    assert_eq!(made["project"], "shop");

    // The same name, however it's written, updates.
    let updated = ok(
        &c,
        "spend_account_upsert",
        json!({ "name": "Gpu", "budget": 50.0, "cost_per_hour": 4.0, "description": "bigger pod" }),
    )
    .await;
    assert_eq!(updated["account_id"], made["account_id"]);
    assert_eq!(updated["budget"], 50.0);
    assert_eq!(updated["cost_per_hour"], 4.0);
    assert_eq!(updated["description"], "bigger pod");

    let listed = ok(&c, "spend_accounts", json!({})).await;
    assert_eq!(names(&listed, "accounts", "name"), ["gpu"]);

    // A recorded amount.
    let entry = ok(
        &c,
        "spend_record",
        json!({ "agent": "claude@shop", "account": "gpu", "amount": 3.0, "note": "api burst" }),
    )
    .await;
    assert_eq!(entry["amount"], 3.0);
    let (code, _) = err(
        &c,
        "spend_record",
        json!({ "agent": "claude@shop", "account": "gpu", "amount": -1.0 }),
    )
    .await;
    assert_eq!(code, "invalid");

    // A lease: open, listed, released, gone unless all=true.
    let lease = ok(
        &c,
        "spend_borrow",
        json!({ "agent": "claude@shop", "account": "gpu", "ttl_seconds": 600, "hours": 1.0, "note": "train" }),
    )
    .await;
    assert_eq!(lease["decision_id"], Value::Null);
    assert_eq!(lease["lease"]["status"], "active");
    let id = lease["lease"]["lease_id"].as_str().unwrap().to_string();
    let open = ok(&c, "spend_leases", json!({})).await;
    assert_eq!(
        names(&open, "leases", "lease_id"),
        std::slice::from_ref(&id)
    );

    let report = ok(&c, "spend_report", json!({ "account": "gpu" })).await;
    let a = &report["accounts"][0];
    assert_eq!(a["spent"], 3.0);
    assert_eq!(a["budget"], 50.0);
    assert_eq!(a["active_leases"], 1);
    assert!(report["markdown"].as_str().unwrap().contains("gpu"));

    let (code, msg) = err(
        &c,
        "spend_release",
        json!({ "agent": "codex@shop", "lease_id": id }),
    )
    .await;
    assert_eq!(code, "forbidden");
    assert!(msg.contains("belongs to"), "{msg}");
    let released = ok(
        &c,
        "spend_release",
        json!({ "agent": "claude@shop", "lease_id": id }),
    )
    .await;
    assert_eq!(released["status"], "ended");
    let (code, _) = err(
        &c,
        "spend_release",
        json!({ "agent": "claude@shop", "lease_id": id }),
    )
    .await;
    assert_eq!(code, "conflict");
    let open = ok(&c, "spend_leases", json!({})).await;
    assert!(open["leases"].as_array().unwrap().is_empty());
    let all = ok(&c, "spend_leases", json!({ "all": true })).await;
    assert_eq!(all["leases"].as_array().unwrap().len(), 1);

    // Bad filters are errors, not empty reports.
    let (code, _) = err(&c, "spend_report", json!({ "period": "soon" })).await;
    assert_eq!(code, "invalid");
    let (code, _) = err(&c, "spend_report", json!({ "lane": "nope" })).await;
    assert_eq!(code, "not_found");
    let (code, _) = err(&c, "spend_report", json!({ "agent": "nope" })).await;
    assert_eq!(code, "not_found");
}

/// BUGS-FOUND.md #12: an update takes only some of the fields, and drops the rest silently.
#[tokio::test(flavor = "multi_thread")]
async fn bug_upserting_an_account_ignores_fields_it_cannot_change() {
    let f = fixture().await;
    let c = f.local().await;
    ok(
        &c,
        "spend_account_upsert",
        json!({ "name": "gpu", "kind": "gpu_pod", "unit": "USD" }),
    )
    .await;
    let r = ok(
        &c,
        "spend_account_upsert",
        json!({ "name": "gpu", "kind": "domain", "unit": "credits", "period": "none", "project": "shop" }),
    )
    .await;
    assert_eq!(
        (&r["kind"], &r["unit"], &r["period"], &r["project"]),
        (
            &json!("gpu_pod"),
            &json!("USD"),
            &json!("monthly"),
            &Value::Null
        ),
        "today's behaviour: reported as a success with nothing changed; should be refused"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_spend_approval_goes_through_a_decision() {
    let f = fixture().await;
    let c = f.local().await;
    ok(
        &c,
        "spend_account_upsert",
        json!({ "name": "gpu", "kind": "gpu_pod", "unit": "USD", "cost_per_hour": 2.0, "requires_approval": true, "approver": "owner", "project": "shop" }),
    )
    .await;
    let lease = ok(
        &c,
        "spend_borrow",
        json!({ "agent": "claude@shop", "account": "gpu" }),
    )
    .await;
    assert_eq!(lease["lease"]["status"], "pending_approval");
    let decision = lease["decision_id"].as_str().unwrap().to_string();
    let answered = ok(
        &c,
        "decision_answer",
        json!({ "decision": decision, "value": "approve", "relayed_by": "manager" }),
    )
    .await;
    assert_eq!(answered["status"], "answered");
    let open = ok(&c, "spend_leases", json!({})).await;
    assert_eq!(open["leases"][0]["status"], "active");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_sees_and_spends_only_within_its_projects() {
    let f = fixture().await;
    account(&f.store, "shop-gpu", Some("shop"));
    account(&f.store, "vault-gpu", Some("vault"));
    account(&f.store, "shared", None);
    let shop = f.remote("claude@shop", "/work/shop").await;

    // Accounts are configured locally, whoever asks.
    let (code, msg) = err(
        &shop,
        "spend_account_upsert",
        json!({ "name": "shop-gpu", "budget": 1.0 }),
    )
    .await;
    assert_eq!(code, "forbidden");
    assert!(msg.contains("configured locally"), "{msg}");

    let listed = ok(&shop, "spend_accounts", json!({})).await;
    let mut listed = names(&listed, "accounts", "name");
    listed.sort();
    assert_eq!(listed, ["shared", "shop-gpu"]);

    // Acting as anyone else is refused; leaving the agent out acts as the token's.
    let (code, _) = err(
        &shop,
        "spend_record",
        json!({ "agent": "codex@shop", "account": "shop-gpu", "amount": 1.0 }),
    )
    .await;
    assert_eq!(code, "forbidden");
    let entry = ok(
        &shop,
        "spend_record",
        json!({ "account": "shop-gpu", "amount": 1.0 }),
    )
    .await;
    assert_eq!(entry["agent"], "claude@shop");

    // A hidden account is the same as a missing one.
    let hidden = err(
        &shop,
        "spend_record",
        json!({ "account": "vault-gpu", "amount": 1.0 }),
    )
    .await;
    assert_eq!(hidden.0, "not_found", "{hidden:?}");
    let missing = err(
        &shop,
        "spend_record",
        json!({ "account": "no-such", "amount": 1.0 }),
    )
    .await;
    assert_eq!(missing.0, "not_found");
    assert_eq!(
        hidden.1.replace("vault-gpu", "X"),
        missing.1.replace("no-such", "X")
    );
    let (code, _) = err(&shop, "spend_borrow", json!({ "account": "vault-gpu" })).await;
    assert_eq!(code, "not_found");

    // Another project's use of a shared account isn't shown, though it counts for the budget.
    f.store
        .record_spend(
            RecordSpend {
                agent: Some("vaulter@vault".into()),
                account: "shared".into(),
                amount: 40.0,
                ..Default::default()
            },
            None,
        )
        .unwrap();
    let report = ok(&shop, "spend_report", json!({ "account": "shared" })).await;
    assert_eq!(report["accounts"][0]["spent"], 0.0);
    assert_eq!(report["accounts"][0]["partial"], true);
    assert!(report["by_agent"].as_array().unwrap().is_empty());

    // Leases of another project look missing.
    let vault_lease = f
        .store
        .borrow_spend(
            BorrowSpend {
                agent: "vaulter@vault".into(),
                account: "vault-gpu".into(),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    let id = vault_lease.lease.lease_id;
    assert!(ok(&shop, "spend_leases", json!({})).await["leases"]
        .as_array()
        .unwrap()
        .is_empty());
    let (code, _) = err(&shop, "spend_release", json!({ "lease_id": id })).await;
    assert_eq!(code, "not_found");
}

// ---------------------------------------------------------------------------------------
// Lanes

#[tokio::test(flavor = "multi_thread")]
async fn the_lane_tools_cover_a_lanes_life() {
    let f = fixture().await;
    let c = f.local().await;
    let lane = ok(
        &c,
        "lane_create",
        json!({ "agent": "manager", "name": "train", "project": "shop", "lead": "claude@shop", "charter": "Ship it." }),
    )
    .await;
    let id = lane["lane"]["lane_id"].as_str().unwrap().to_string();
    assert_eq!(lane["lane"]["status"], "planned");
    let (code, _) = err(
        &c,
        "lane_create",
        json!({ "agent": "manager", "name": "train", "project": "shop", "lead": "claude@shop" }),
    )
    .await;
    assert_eq!(code, "conflict", "a name is taken while the lane is open");
    let (code, _) = err(
        &c,
        "lane_create",
        json!({ "agent": "manager", "name": "other", "project": "nowhere", "lead": "claude@shop" }),
    )
    .await;
    assert_eq!(code, "not_found");

    // Links of each kind.
    let mail = f
        .store
        .send_message(SendMessage {
            from: "claude@shop".into(),
            to: vec!["codex@shop".into()],
            subject: "plan".into(),
            body: "the plan".into(),
            request: Some(NewRequest::default()),
            ..Default::default()
        })
        .unwrap();
    let request = f
        .store
        .requests(RequestFilter::default())
        .unwrap()
        .remove(0)
        .request_id;
    let hold = ok(
        &c,
        "hold_paths",
        json!({ "agent": "claude@shop", "project": "shop", "paths": ["src/lib.rs"], "exclusive": true, "lane": "-" }),
    )
    .await;
    let hold = hold["granted"][0]["hold_id"].as_str().unwrap().to_string();
    let updated = ok(
        &c,
        "lane_update",
        json!({
            "agent": "manager",
            "lane": id,
            "status": "working",
            "charter": "Ship it, carefully.",
            "lead": "codex@shop",
            "link_threads": [mail.thread_id],
            "link_requests": [request],
            "link_holds": [hold],
            "link_messages": [mail.message_id],
        }),
    )
    .await;
    assert_eq!(updated["lane"]["status"], "working");
    assert_eq!(updated["lane"]["charter"], "Ship it, carefully.");
    assert_eq!(updated["lane"]["lead"], "codex@shop");
    assert_eq!(updated["links"].as_array().unwrap().len(), 4);
    let (code, _) = err(
        &c,
        "lane_update",
        json!({ "agent": "manager", "lane": id, "link_threads": ["NO-SUCH-THREAD"] }),
    )
    .await;
    assert_eq!(code, "not_found");

    // Listing filters.
    ok(
        &c,
        "lane_create",
        json!({ "agent": "manager", "name": "vault-work", "project": "vault", "lead": "vaulter@vault", "status": "blocked" }),
    )
    .await;
    let all = ok(&c, "lane_list", json!({})).await;
    assert_eq!(all["lanes"].as_array().unwrap().len(), 2);
    let by_status = ok(&c, "lane_list", json!({ "status": "blocked" })).await;
    assert_eq!(names(&by_status, "lanes", "name"), ["vault-work"]);
    let by_project = ok(&c, "lane_list", json!({ "project": "shop" })).await;
    assert_eq!(names(&by_project, "lanes", "name"), ["train"]);
    let by_lead = ok(&c, "lane_list", json!({ "lead": "codex@shop" })).await;
    assert_eq!(names(&by_lead, "lanes", "name"), ["train"]);
    let (code, _) = err(&c, "lane_list", json!({ "project": "nowhere" })).await;
    assert_eq!(code, "not_found");

    // Holds by lane, with filters.
    let holds = ok(&c, "holds_by_lane", json!({ "project": "shop" })).await;
    assert_eq!(holds["lanes"].as_array().unwrap().len(), 1);
    assert_eq!(holds["lanes"][0]["holds"][0]["pattern"], "src/lib.rs");
    let none = ok(&c, "holds_by_lane", json!({ "agent": "codex@shop" })).await;
    assert!(none["lanes"].as_array().unwrap().is_empty());

    // A status report; a progress over 100 is refused.
    let report = ok(
        &c,
        "lane_report",
        json!({ "agent": "codex@shop", "lane": id, "summary": "halfway", "progress": 50, "status": "blocked", "links": ["https://example.test/1"], "artifacts": ["out.bin"] }),
    )
    .await;
    assert_eq!(report["kind"], "status");
    assert_eq!(report["progress"], 50);
    let (code, _) = err(
        &c,
        "lane_report",
        json!({ "agent": "codex@shop", "lane": id, "summary": "too far", "progress": 101 }),
    )
    .await;
    assert_eq!(code, "invalid");
    let shown = ok(&c, "lane_show", json!({ "lane": id })).await;
    assert_eq!(shown["reports"][0]["summary"], "halfway");
    assert_eq!(shown["lane"]["status"], "blocked");

    // kind=done closes it, and it leaves the default list.
    let done = ok(
        &c,
        "lane_report",
        json!({ "agent": "codex@shop", "lane": id, "kind": "done", "summary": "shipped", "outcome": "merged", "pr_url": "https://example.test/pr/1" }),
    )
    .await;
    assert_eq!(done["kind"], "done");
    assert_eq!(
        ok(&c, "lane_show", json!({ "lane": id })).await["lane"]["status"],
        "done"
    );
    let open = ok(&c, "lane_list", json!({})).await;
    assert_eq!(names(&open, "lanes", "name"), ["vault-work"]);
    let closed = ok(&c, "lane_list", json!({ "include_closed": true })).await;
    assert_eq!(closed["lanes"].as_array().unwrap().len(), 2);

    // lane_close: abandoned or done, once.
    let vault = ok(
        &c,
        "lane_close",
        json!({ "agent": "manager", "lane": "vault-work", "abandon": true, "outcome": "not needed" }),
    )
    .await;
    assert_eq!(vault["lane"]["status"], "abandoned");
    let (code, _) = err(
        &c,
        "lane_close",
        json!({ "agent": "manager", "lane": "vault-work" }),
    )
    .await;
    assert_eq!(code, "conflict");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tokens_lane_tools_stop_at_its_projects() {
    let f = fixture().await;
    let local = f.local().await;
    let vault_lane = ok(
        &local,
        "lane_create",
        json!({ "agent": "manager", "name": "secret", "project": "vault", "lead": "vaulter@vault", "charter": "Nobody sees this.", "status": "working" }),
    )
    .await;
    let vault_id = vault_lane["lane"]["lane_id"].as_str().unwrap().to_string();
    ok(
        &local,
        "hold_paths",
        json!({ "agent": "vaulter@vault", "project": "vault", "paths": ["k.pem"], "exclusive": true, "lane": vault_id }),
    )
    .await;

    let shop = f.remote("claude@shop", "/work/shop").await;
    let (code, msg) = err(
        &shop,
        "lane_create",
        json!({ "name": "mine", "project": "vault", "lead": "claude@shop" }),
    )
    .await;
    assert_eq!(code, "forbidden");
    assert!(msg.contains("outside this token's"), "{msg}");
    let (code, msg) = err(
        &shop,
        "lane_create",
        json!({ "name": "mine", "lead": "claude@shop" }),
    )
    .await;
    assert_eq!(code, "forbidden");
    assert!(msg.contains("must name a project"), "{msg}");
    let (code, _) = err(
        &shop,
        "lane_create",
        json!({ "agent": "codex@shop", "name": "mine", "project": "shop", "lead": "claude@shop" }),
    )
    .await;
    assert_eq!(code, "forbidden", "acting as another agent");
    let mine = ok(
        &shop,
        "lane_create",
        json!({ "name": "mine", "project": "shop", "lead": "claude@shop" }),
    )
    .await;
    assert_eq!(mine["lane"]["project"], "shop");

    // Reads leave out the other project.
    let listed = ok(&shop, "lane_list", json!({ "include_closed": true })).await;
    assert_eq!(names(&listed, "lanes", "name"), ["mine"]);
    let holds = ok(&shop, "holds_by_lane", json!({})).await;
    assert!(holds["lanes"].as_array().unwrap().is_empty());

    // A hidden lane reads the same as a missing one, by id and by name, for every tool.
    for key in [vault_id.as_str(), "secret"] {
        let (code, msg) = err(&shop, "lane_show", json!({ "lane": key })).await;
        assert_eq!(code, "not_found", "{key}: {msg}");
        assert!(!msg.contains("vault"), "{msg}");
        let (code, _) = err(
            &shop,
            "lane_update",
            json!({ "lane": key, "charter": "taken" }),
        )
        .await;
        assert_eq!(code, "not_found");
        let (code, _) = err(
            &shop,
            "lane_report",
            json!({ "lane": key, "summary": "taken" }),
        )
        .await;
        assert_eq!(code, "not_found");
        let (code, _) = err(&shop, "lane_close", json!({ "lane": key })).await;
        assert_eq!(code, "not_found");
    }
    let detail = f.store.lane(&vault_id, None).unwrap();
    assert_eq!(detail.lane.charter, "Nobody sees this.");
    assert_eq!(detail.lane.status, LaneStatus::Working);
    assert!(detail.reports.is_empty());
}

// ---------------------------------------------------------------------------------------
// Decisions

fn ask_args(agent: &str) -> Value {
    json!({
        "agent": agent,
        "question": "Ship it?",
        "options": [
            { "value": "yes", "label": "Yes" },
            { "value": "no", "label": "No" },
        ],
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn the_decision_tools_cover_a_questions_life() {
    let f = fixture().await;
    let c = f.local().await;
    let d = ok(&c, "decision_ask", ask_args("claude@shop")).await;
    let id = d["decision_id"].as_str().unwrap().to_string();
    let listed = ok(&c, "decision_list", json!({})).await;
    assert_eq!(
        names(&listed, "decisions", "decision_id"),
        std::slice::from_ref(&id)
    );
    let shown = ok(&c, "decision_show", json!({ "decision": id })).await;
    assert!(shown["markdown"].as_str().unwrap().contains("Ship it?"));
    assert_eq!(shown["decision"]["status"], "open");

    // Filters.
    for (args, want) in [
        (json!({ "project": "shop" }), 1),
        (json!({ "project": "vault" }), 0),
        (json!({ "status": "answered" }), 0),
        (json!({ "all": true, "limit": 1 }), 1),
        (json!({ "human": "nobody" }), 0),
    ] {
        let got = ok(&c, "decision_list", args.clone()).await;
        assert_eq!(got["decisions"].as_array().unwrap().len(), want, "{args}");
    }

    // Only a manager relays; only the asker withdraws.
    let (code, _) = err(
        &c,
        "decision_answer",
        json!({ "decision": id, "value": "yes", "relayed_by": "codex@shop" }),
    )
    .await;
    assert_eq!(code, "forbidden");
    let (code, _) = err(
        &c,
        "decision_cancel",
        json!({ "agent": "codex@shop", "decision": id }),
    )
    .await;
    assert_eq!(code, "forbidden");
    let (code, _) = err(
        &c,
        "decision_answer",
        json!({ "decision": id, "value": "maybe", "relayed_by": "manager" }),
    )
    .await;
    assert_eq!(code, "invalid");

    let answered = ok(
        &c,
        "decision_answer",
        json!({ "decision": id, "value": "yes", "text": "go on", "answered_by": "owner", "relayed_by": "manager" }),
    )
    .await;
    assert_eq!(answered["status"], "answered");
    assert_eq!(answered["answer"]["value"], "yes");

    // A second one, withdrawn with a reason.
    let second = ok(&c, "decision_ask", ask_args("claude@shop")).await;
    let cancelled = ok(
        &c,
        "decision_cancel",
        json!({ "agent": "claude@shop", "decision": second["decision_id"], "reason": "moot" }),
    )
    .await;
    assert_eq!(cancelled["status"], "cancelled");
    let (code, _) = err(
        &c,
        "decision_cancel",
        json!({ "agent": "claude@shop", "decision": second["decision_id"] }),
    )
    .await;
    assert_eq!(code, "conflict");
    let (code, _) = err(&c, "decision_show", json!({ "decision": "NOPE" })).await;
    assert_eq!(code, "not_found");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tokens_decision_tools_stop_at_its_projects() {
    let f = fixture().await;
    let local = f.local().await;
    let vault = ok(&local, "decision_ask", ask_args("vaulter@vault")).await;
    let id = vault["decision_id"].as_str().unwrap().to_string();
    let shop = f.remote("manager", "/work/shop").await;

    let listed = ok(&shop, "decision_list", json!({ "all": true })).await;
    assert!(listed["decisions"].as_array().unwrap().is_empty());
    let (code, _) = err(&shop, "decision_show", json!({ "decision": id })).await;
    assert_eq!(code, "not_found");
    let (code, _) = err(
        &shop,
        "decision_answer",
        json!({ "decision": id, "value": "yes" }),
    )
    .await;
    assert_eq!(code, "not_found");
    assert_eq!(
        f.store.decision(&id, None).unwrap().status,
        DecisionStatus::Open
    );

    // The relayer is always the token's agent, whatever the call says.
    let (code, _) = err(
        &shop,
        "decision_answer",
        json!({ "decision": id, "value": "yes", "relayed_by": "claude@shop" }),
    )
    .await;
    assert_eq!(code, "forbidden");
    let (code, _) = err(
        &shop,
        "decision_ask",
        json!({ "agent": "claude@shop", "question": "?", "options": [] }),
    )
    .await;
    assert_eq!(code, "forbidden");
    let (code, _) = err(
        &shop,
        "decision_cancel",
        json!({ "agent": "vaulter@vault", "decision": id }),
    )
    .await;
    assert_eq!(code, "forbidden");
}

// ---------------------------------------------------------------------------------------
// Resources and what a token may call

#[tokio::test(flavor = "multi_thread")]
async fn the_resource_tools_cover_a_servers_life() {
    let f = fixture().await;
    let c = f.local().await;
    let reg = ok(
        &c,
        "resource_register",
        json!({ "agent": "claude@shop", "kind": "model_server", "name": "ollama@11434", "endpoint": "http://127.0.0.1:11434", "pid": 4242, "project": "shop", "purpose": "local llm", "ttl_seconds": 600, "metadata": { "backend": "metal" } }),
    )
    .await;
    assert_eq!(reg["outcome"], "registered");
    assert_eq!(reg["resource"]["port"], 11434);
    let id = reg["resource"]["resource_id"].as_str().unwrap().to_string();

    for (args, want) in [
        (json!({ "query": "OLLAMA" }), 1),
        (json!({ "kind": "model_server", "project": "shop" }), 1),
        (json!({ "kind": "gpu" }), 0),
        (json!({ "query": "nothing-like-it" }), 0),
        (json!({ "local_only": true }), 1),
    ] {
        let got = ok(&c, "resource_find", args.clone()).await;
        assert_eq!(got["resources"].as_array().unwrap().len(), want, "{args}");
    }

    let borrowed = ok(
        &c,
        "resource_borrow",
        json!({ "agent": "codex@shop", "resource": "model_server:ollama@11434", "ttl_seconds": 600 }),
    )
    .await;
    assert_eq!(borrowed["outcome"], "borrowed");
    assert_eq!(borrowed["resource"]["refcount"], 2);
    let (code, _) = err(
        &c,
        "resource_borrow",
        json!({ "agent": "codex@shop", "resource": "not a resource ref at all:" }),
    )
    .await;
    assert_ne!(code, "");

    let beat = ok(
        &c,
        "resource_heartbeat",
        json!({ "agent": "codex@shop", "resources": [id] }),
    )
    .await;
    assert_eq!(beat["resources"].as_array().unwrap().len(), 1);
    let all = ok(
        &c,
        "resource_heartbeat",
        json!({ "agent": "claude@shop", "resources": [] }),
    )
    .await;
    assert_eq!(all["resources"].as_array().unwrap().len(), 1);

    // Ownership passes on; the new owner stops it.
    let released = ok(
        &c,
        "resource_release",
        json!({ "agent": "claude@shop", "resource": id }),
    )
    .await;
    assert_eq!(released["resource"]["owner"], "codex@shop");
    let stopped = ok(
        &c,
        "resource_release",
        json!({ "agent": "codex@shop", "resource": id, "stopped": true }),
    )
    .await;
    assert_eq!(stopped["resource"]["status"], "released");
    let gone = ok(&c, "resource_find", json!({})).await;
    assert!(gone["resources"].as_array().unwrap().is_empty());
    let kept = ok(&c, "resource_find", json!({ "include_released": true })).await;
    assert_eq!(kept["resources"].as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_is_held_to_its_agent_and_projects_on_the_other_tools() {
    let f = fixture().await;
    let shop = f.remote("claude@shop", "/work/shop").await;
    let forbidden = |(code, msg): (String, String), what: &str| {
        assert_eq!(code, "forbidden", "{what}: {msg}");
        msg
    };

    // Never available remotely.
    let m = forbidden(
        err(
            &shop,
            "resource_reap",
            json!({ "agent": "claude@shop", "resource": "x" }),
        )
        .await,
        "reap",
    );
    assert!(m.contains("never available"), "{m}");

    // Registering: only its own name, in its own project.
    let m = forbidden(
        err(
            &shop,
            "register_agent",
            json!({ "name": "someone-else", "program": "claude", "project": "shop" }),
        )
        .await,
        "another name",
    );
    assert!(m.contains("can only register claude@shop"), "{m}");
    let m = forbidden(
        err(
            &shop,
            "register_agent",
            json!({ "name": "claude", "program": "claude" }),
        )
        .await,
        "no project",
    );
    assert!(m.contains("can only register"), "{m}");
    forbidden(
        err(
            &shop,
            "register_agent",
            json!({ "name": "claude", "program": "claude", "project": "vault" }),
        )
        .await,
        "another project",
    );
    let again = ok(
        &shop,
        "register_agent",
        json!({ "name": "claude", "program": "claude", "project": "/work/shop" }),
    )
    .await;
    assert_eq!(again["address"], "claude@shop");

    // A token for an agent with no project registers without one, and may not name one.
    let bare = f.remote("manager", "**").await;
    forbidden(
        err(
            &bare,
            "register_agent",
            json!({ "name": "manager", "program": "grok_bot", "project": "shop" }),
        )
        .await,
        "project on a bare agent",
    );

    // Projects outside the globs.
    forbidden(
        err(&shop, "ensure_project", json!({ "path": "/work/vault" })).await,
        "ensure outside",
    );
    forbidden(
        err(&shop, "ensure_project", json!({})).await,
        "ensure with no path",
    );
    forbidden(
        err(
            &shop,
            "hold_paths",
            json!({ "project": "vault", "paths": ["a"], "exclusive": true }),
        )
        .await,
        "hold outside",
    );
    forbidden(
        err(
            &shop,
            "resource_register",
            json!({ "kind": "port", "name": "tcp:1", "project": "vault" }),
        )
        .await,
        "resource outside",
    );

    // Another agent's mailbox, resources and heartbeat.
    for (tool, args) in [
        ("fetch_inbox", json!({ "agent": "codex@shop" })),
        ("heartbeat", json!({ "agent": "codex@shop" })),
        (
            "resource_borrow",
            json!({ "agent": "codex@shop", "resource": "x" }),
        ),
        (
            "resource_heartbeat",
            json!({ "agent": "codex@shop", "resources": [] }),
        ),
        (
            "send_message",
            json!({ "from": "codex@shop", "to": ["claude@shop"], "subject": "s", "body": "b" }),
        ),
        ("threads", json!({ "agent": "codex@shop" })),
    ] {
        forbidden(err(&shop, tool, args).await, tool);
    }

    // A thread it isn't part of.
    let private = f
        .store
        .send_message(SendMessage {
            from: "codex@shop".into(),
            to: vec!["vaulter@vault".into()],
            subject: "private".into(),
            body: "private".into(),
            ..Default::default()
        })
        .unwrap();
    let m = forbidden(
        err(&shop, "threads", json!({ "thread_id": private.thread_id })).await,
        "thread",
    );
    assert!(m.contains("threads they are part of"), "{m}");

    // Refusals leave a trail.
    let audit = f.store.audit_log(200).unwrap();
    assert!(audit
        .iter()
        .any(|a| a.operation == "tools/call resource_reap" && a.outcome == "denied"));
}
