//! MCP tools exercised through real rmcp clients: in-process and over loopback HTTP.

use another::mcp::AnotherMcp;
use another_core::{Clock, Store};
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

type Client = RunningService<RoleClient, ClientConfig>;

async fn call(client: &Client, tool: &str, args: Value) -> CallToolResult {
    let args = args.as_object().cloned().expect("object args");
    client
        .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(args))
        .await
        .expect("tool call")
}

async fn call_ok(client: &Client, tool: &str, args: Value) -> Value {
    let res = call(client, tool, args).await;
    assert_ne!(res.is_error, Some(true), "{tool} failed: {res:?}");
    res.structured_content.expect("structured output")
}

/// Error results carry `{"error": {"code", "message"}}` as text.
async fn call_err(client: &Client, tool: &str, args: Value) -> String {
    let res = call(client, tool, args).await;
    assert_eq!(res.is_error, Some(true), "{tool} should fail: {res:?}");
    let text = serde_json::to_value(&res.content).unwrap()[0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    let body: Value = serde_json::from_str(&text).unwrap();
    body["error"]["code"].as_str().unwrap().to_string()
}

async fn in_process(store: Store) -> Client {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let server = AnotherMcp::legacy_repair(store)
            .serve(server_io)
            .await
            .unwrap();
        let _ = server.waiting().await;
    });
    ClientConfig::default().serve(client_io).await.unwrap()
}

#[tokio::test]
async fn heartbeat_rejects_start_and_end_without_releasing_holds() {
    let dir = tempfile::tempdir().unwrap();
    let project_path = dir.path().join("app");
    let store = Store::open_in_memory(Clock::System).unwrap();
    let client = in_process(store.clone()).await;
    call_ok(&client, "ensure_project", json!({ "path": project_path })).await;
    call_ok(
        &client,
        "register_agent",
        json!({ "name": "codex-1", "program": "codex" }),
    )
    .await;
    let started = call_ok(
        &client,
        "heartbeat",
        json!({ "agent": "codex-1", "start": true }),
    )
    .await;
    assert_eq!(started["status"], "active");
    let held = call_ok(
        &client,
        "hold_paths",
        json!({ "agent": "codex-1", "project": project_path, "paths": ["src/**"] }),
    )
    .await;
    assert_eq!(held["granted"].as_array().unwrap().len(), 1);

    let error = call_error(
        &client,
        "heartbeat",
        json!({ "agent": "codex-1", "start": true, "end": true }),
    )
    .await;
    assert_eq!(error["code"], "invalid");
    let message = error["message"].as_str().unwrap();
    assert!(
        message.contains("start") && message.contains("end"),
        "{error}"
    );
    let holds = call_ok(
        &client,
        "hold_paths",
        json!({ "agent": "codex-1", "action": "list" }),
    )
    .await;
    assert_eq!(holds["holds"].as_array().unwrap().len(), 1);
    assert_eq!(store.agent("codex-1").unwrap().status, "active");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn exposes_the_tool_surface() {
    let client = in_process(Store::open_in_memory(Clock::System).unwrap()).await;
    let info = client.peer_info().expect("server info");
    assert_eq!(info.server_info.as_ref().unwrap().name, "another");
    let tools = client.list_all_tools().await.unwrap();
    for name in [
        "whats_new",
        "activity_feed",
        "activity_summarize",
        "work_board",
    ] {
        let tool = tools.iter().find(|t| t.name == name).unwrap();
        let schema = serde_json::to_value(&tool.input_schema).unwrap();
        let description = schema["properties"]["since"]["description"]
            .as_str()
            .unwrap();
        assert!(
            description.contains("bare seconds"),
            "{name}: {description}"
        );
    }
    let mut names: Vec<String> = tools.into_iter().map(|t| t.name.to_string()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "acknowledge",
            "activity_feed",
            "activity_summarize",
            "another_identity",
            "another_lease",
            "claim_request",
            "ensure_project",
            "fetch_inbox",
            "heartbeat",
            "hold_paths",
            "orphans_list",
            "queue_list",
            "queue_next",
            "queue_propose",
            "queue_update",
            "register_agent",
            "resource_borrow",
            "resource_find",
            "resource_heartbeat",
            "resource_reap",
            "resource_register",
            "resource_release",
            "send_message",
            "threads",
            "whats_new",
            "work_board"
        ]
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn full_flow_over_mcp() {
    let dir = tempfile::tempdir().unwrap();
    let project_path = dir.path().join("site");
    let client = in_process(Store::open_in_memory(Clock::System).unwrap()).await;

    let project = call_ok(&client, "ensure_project", json!({ "path": project_path })).await;
    assert_eq!(project["slug"], "site");
    for (name, program) in [("claude-1", "claude"), ("codex-1", "codex")] {
        let a = call_ok(
            &client,
            "register_agent",
            json!({ "name": name, "program": program }),
        )
        .await;
        assert_eq!(a["status"], "active");
    }
    // Unknown programs are rejected by the input schema.
    let bad = client
        .call_tool(
            CallToolRequestParams::new("register_agent").with_arguments(
                json!({ "name": "x", "program": "notreal" })
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .await;
    assert!(bad.is_err() || bad.unwrap().is_error == Some(true));

    // send -> inbox -> ack
    let sent = call_ok(
        &client,
        "send_message",
        json!({
            "from": "claude-1", "to": ["codex-1"], "subject": "Migrate DB",
            "body": "Run the migration please", "importance": "urgent",
            "create_request": true
        }),
    )
    .await;
    let message_id = sent["message_id"].as_str().unwrap().to_string();
    let request_id = sent["request_id"].as_str().unwrap().to_string();

    let inbox = call_ok(&client, "fetch_inbox", json!({ "agent": "codex-1" })).await;
    assert_eq!(inbox["unread"], 1);
    assert_eq!(inbox["urgent_unread"], 1);
    assert_eq!(inbox["messages"][0]["subject"], "Migrate DB");
    assert_eq!(inbox["requests"][0]["status"], "open");

    let ack = call_ok(
        &client,
        "acknowledge",
        json!({ "agent": "codex-1", "message_ids": [message_id] }),
    )
    .await;
    assert_eq!(ack["updated"].as_array().unwrap().len(), 1);
    let inbox = call_ok(&client, "fetch_inbox", json!({ "agent": "codex-1" })).await;
    assert_eq!(inbox["unread"], 0);
    assert!(inbox["messages"].as_array().unwrap().is_empty());

    // reply without explicit recipients goes back to the sender, in-thread
    let reply = call_ok(
        &client,
        "send_message",
        json!({ "from": "codex-1", "reply_to": message_id, "body": "on it" }),
    )
    .await;
    assert_eq!(reply["to"], json!(["claude-1"]));
    assert_eq!(reply["subject"], "Re: Migrate DB");
    let thread = call_ok(
        &client,
        "threads",
        json!({ "thread_id": sent["thread_id"] }),
    )
    .await;
    assert_eq!(thread["thread"]["messages"].as_array().unwrap().len(), 2);
    let threads = call_ok(&client, "threads", json!({ "agent": "claude-1" })).await;
    assert_eq!(threads["threads"][0]["messages"], 2);

    // request claim -> done
    let claimed = call_ok(
        &client,
        "claim_request",
        json!({ "agent": "codex-1", "request_id": request_id }),
    )
    .await;
    assert_eq!(claimed["status"], "claimed");
    assert_eq!(
        call_err(
            &client,
            "claim_request",
            json!({ "agent": "claude-1", "request_id": request_id })
        )
        .await,
        "conflict"
    );
    let done = call_ok(
        &client,
        "claim_request",
        json!({ "agent": "codex-1", "request_id": request_id, "action": "complete", "note": "migrated" }),
    )
    .await;
    assert_eq!(done["status"], "done");

    // hold acquire -> conflict -> release
    let got = call_ok(
        &client,
        "hold_paths",
        json!({ "agent": "claude-1", "project": project_path, "paths": ["src/**"], "ttl_seconds": 600 }),
    )
    .await;
    assert_eq!(got["granted"].as_array().unwrap().len(), 1);
    let clash = call_ok(
        &client,
        "hold_paths",
        json!({ "agent": "codex-1", "project": "site", "paths": ["src/app.ts"] }),
    )
    .await;
    assert!(clash["granted"].as_array().unwrap().is_empty());
    assert_eq!(clash["conflicts"][0]["held"]["agent"], "claude-1");
    let check = call_ok(
        &client,
        "hold_paths",
        json!({ "agent": "codex-1", "action": "check", "project": "site", "paths": ["README.md"] }),
    )
    .await;
    assert!(check["conflicts"].as_array().unwrap().is_empty());
    let listed = call_ok(
        &client,
        "hold_paths",
        json!({ "agent": "codex-1", "action": "list" }),
    )
    .await;
    assert_eq!(listed["holds"].as_array().unwrap().len(), 1);

    let hb = call_ok(&client, "heartbeat", json!({ "agent": "claude-1" })).await;
    assert!(hb["renewed_holds"].as_array().unwrap().is_empty());
    let end = call_ok(
        &client,
        "heartbeat",
        json!({ "agent": "claude-1", "end": true }),
    )
    .await;
    assert_eq!(end["status"], "offline");
    assert_eq!(end["released_holds"].as_array().unwrap().len(), 1);
    let retry = call_ok(
        &client,
        "hold_paths",
        json!({ "agent": "codex-1", "project": "site", "paths": ["src/app.ts"] }),
    )
    .await;
    assert_eq!(retry["granted"].as_array().unwrap().len(), 1);

    let feed = call_ok(
        &client,
        "activity_feed",
        json!({ "kinds": ["request"], "since": "1h" }),
    )
    .await;
    let kinds: Vec<&str> = feed["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["request.created", "request.claimed", "request.completed"]
    );
    let summary = call_ok(&client, "activity_summarize", json!({})).await;
    assert_eq!(summary["requests"]["open"], 0);
    assert!(summary["text"].as_str().unwrap().contains("event(s) since"));

    // The board is read-only: calling it adds no event.
    let before = call_ok(&client, "activity_feed", json!({ "limit": 500 })).await["items"]
        .as_array()
        .unwrap()
        .len();
    let board = call_ok(&client, "work_board", json!({ "project": "site" })).await;
    assert_eq!(board["project"], "site");
    assert_eq!(board["projects"][0]["project"], "site");
    assert_eq!(
        board["requests"].as_array().unwrap().len(),
        0,
        "the request is done"
    );
    let after = call_ok(&client, "activity_feed", json!({ "limit": 500 })).await["items"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(before, after);
    assert_eq!(
        call_err(
            &client,
            "work_board",
            json!({ "since": "01ARZ3NDEKTSV4RRFFQ69G5FAV" })
        )
        .await,
        "invalid"
    );
    assert_eq!(
        call_err(&client, "work_board", json!({ "project": "nope" })).await,
        "not_found"
    );

    assert_eq!(
        call_err(&client, "fetch_inbox", json!({ "agent": "ghost" })).await,
        "not_found"
    );
    client.cancel().await.unwrap();
}

/// `fetch_inbox` is not an ack, `acknowledge` records only the acting agent's receipt, and
/// `ack: false` marks read without acknowledging.
#[tokio::test]
async fn fetch_inbox_is_not_an_ack_and_acknowledge_is_per_agent() {
    let client = in_process(Store::open_in_memory(Clock::System).unwrap()).await;
    for (name, program) in [
        ("claude-1", "claude"),
        ("codex-1", "codex"),
        ("grok-1", "grok"),
    ] {
        call_ok(
            &client,
            "register_agent",
            json!({ "name": name, "program": program }),
        )
        .await;
    }
    let sent = call_ok(
        &client,
        "send_message",
        json!({
            "from": "claude-1", "to": ["codex-1", "grok-1"], "subject": "Confirm receipt",
            "body": "please ack", "ack_required": true
        }),
    )
    .await;
    let message_id = sent["message_id"].as_str().unwrap().to_string();
    let all = |agent: &str| json!({ "agent": agent, "unread_only": false });

    // Fetching twice leaves the counters and the delivery untouched.
    let first = call_ok(&client, "fetch_inbox", json!({ "agent": "codex-1" })).await;
    assert_eq!(
        (first["unread"].as_u64(), first["awaiting_ack"].as_u64()),
        (Some(1), Some(1))
    );
    let second = call_ok(&client, "fetch_inbox", json!({ "agent": "codex-1" })).await;
    assert_eq!(second["unread"], first["unread"]);
    assert_eq!(second["awaiting_ack"], first["awaiting_ack"]);
    assert_eq!(second["messages"][0]["message_id"], message_id.as_str());
    assert!(second["messages"][0]["read_at"].is_null());
    assert!(second["messages"][0]["acked_at"].is_null());
    let digest = call_ok(&client, "whats_new", json!({ "agent": "codex-1" })).await;
    assert_eq!(
        (digest["unread"].as_u64(), digest["awaiting_ack"].as_u64()),
        (Some(1), Some(1))
    );
    assert_eq!(
        call_ok(&client, "fetch_inbox", json!({ "agent": "codex-1" })).await["unread"],
        1
    );

    // ack: false marks read without acknowledging.
    let read = call_ok(
        &client,
        "acknowledge",
        json!({ "agent": "codex-1", "message_ids": [message_id], "ack": false }),
    )
    .await;
    assert_eq!(read["updated"].as_array().unwrap().len(), 1);
    let codex = call_ok(&client, "fetch_inbox", all("codex-1")).await;
    assert_eq!(
        (codex["unread"].as_u64(), codex["awaiting_ack"].as_u64()),
        (Some(0), Some(1))
    );
    assert!(codex["messages"][0]["read_at"].is_string());
    assert!(codex["messages"][0]["acked_at"].is_null());

    // The other recipient is untouched.
    let grok = call_ok(&client, "fetch_inbox", all("grok-1")).await;
    assert_eq!(
        (grok["unread"].as_u64(), grok["awaiting_ack"].as_u64()),
        (Some(1), Some(1))
    );
    assert!(grok["messages"][0]["read_at"].is_null());
    assert!(grok["messages"][0]["acked_at"].is_null());

    // A real ack records codex-1's receipt only.
    let acked = call_ok(
        &client,
        "acknowledge",
        json!({ "agent": "codex-1", "message_ids": [message_id] }),
    )
    .await;
    assert_eq!(acked["updated"].as_array().unwrap().len(), 1);
    let codex = call_ok(&client, "fetch_inbox", all("codex-1")).await;
    assert_eq!(codex["awaiting_ack"], 0);
    assert!(codex["messages"][0]["acked_at"].is_string());
    let grok = call_ok(&client, "fetch_inbox", all("grok-1")).await;
    assert_eq!(
        (grok["unread"].as_u64(), grok["awaiting_ack"].as_u64()),
        (Some(1), Some(1))
    );
    assert!(grok["messages"][0]["acked_at"].is_null());
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn mark_read_only_preserves_other_recipient_unread() {
    let client = in_process(Store::open_in_memory(Clock::System).unwrap()).await;
    for name in ["sender", "a", "b"] {
        call_ok(
            &client,
            "register_agent",
            json!({ "name": name, "program": "codex" }),
        )
        .await;
    }
    let sent = call_ok(
        &client,
        "send_message",
        json!({
            "from": "sender", "to": ["a", "b"], "subject": "Shared",
            "body": "Read separately", "ack_required": true
        }),
    )
    .await;
    let id = sent["message_id"].as_str().unwrap();
    // fetch_inbox defaults to unread_only: true.
    let before = call_ok(&client, "fetch_inbox", json!({ "agent": "b" })).await;
    assert_eq!(before["unread"], 1);
    assert_eq!(before["messages"][0]["message_id"], id);
    let marked = call_ok(
        &client,
        "acknowledge",
        json!({
            "agent": "a", "message_ids": [id], "ack": false
        }),
    )
    .await;
    assert_eq!(marked["updated"], json!([id]));
    let a = call_ok(&client, "fetch_inbox", json!({ "agent": "a" })).await;
    assert_eq!(a["unread"], 0);
    assert!(a["messages"].as_array().unwrap().is_empty());
    let a = call_ok(
        &client,
        "fetch_inbox",
        json!({ "agent": "a", "unread_only": false }),
    )
    .await;
    assert!(a["messages"][0]["read_at"].is_string());
    assert!(a["messages"][0]["acked_at"].is_null());
    assert_eq!(a["awaiting_ack"], 1);
    let b = call_ok(&client, "fetch_inbox", json!({ "agent": "b" })).await;
    assert_eq!(b["unread"], 1);
    assert_eq!(b["messages"].as_array().unwrap().len(), 1);
    assert_eq!(b["messages"][0]["message_id"], id);
    assert!(b["messages"][0]["read_at"].is_null());
    assert!(b["messages"][0]["acked_at"].is_null());
    assert_eq!(b["awaiting_ack"], 1);
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn streamable_http_on_loopback() {
    let dir = tempfile::tempdir().unwrap();
    let project_path = dir.path().join("site");
    let store = Store::open(dir.path()).unwrap();
    let ct = CancellationToken::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = another::serve::http_router(
        store.clone(),
        ct.clone(),
        &another::serve::HttpOptions {
            legacy_repair: true,
            bound_port: addr.port(),
            ..Default::default()
        },
    );
    let server = tokio::spawn({
        let ct = ct.clone();
        async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move { ct.cancelled_owned().await })
                .await
                .unwrap();
        }
    });

    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp")),
    );
    let client = ClientConfig::default().serve(transport).await.unwrap();
    let agent = call_ok(
        &client,
        "register_agent",
        json!({ "name": "grok-bot", "program": "grok_bot", "project": project_path }),
    )
    .await;
    assert_eq!(agent["address"], "grok-bot@site");
    // Written through HTTP, visible to any other connection on the same database.
    assert_eq!(store.agent("grok-bot").unwrap().program, "grok_bot");

    let before = store.agent("grok-bot").unwrap();
    let error = call_error(
        &client,
        "heartbeat",
        json!({ "agent": "grok-bot", "start": true, "end": true }),
    )
    .await;
    assert_eq!(error["code"], "invalid");
    let after = store.agent("grok-bot").unwrap();
    assert_eq!(after.status, before.status);
    assert_eq!(after.last_seen_at, before.last_seen_at);

    client.cancel().await.unwrap();
    ct.cancel();
    server.await.unwrap();
}

/// The whole error body of a failed call: `{"code", "message", "hint"?}`.
async fn call_error(client: &Client, tool: &str, args: Value) -> Value {
    let res = call(client, tool, args).await;
    assert_eq!(res.is_error, Some(true), "{tool} should fail: {res:?}");
    let text = serde_json::to_value(&res.content).unwrap()[0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    serde_json::from_str::<Value>(&text).unwrap()["error"].clone()
}

/// Q20: a resource's endpoint, purpose and metadata are size limited. Over MCP, in process
/// or through `another serve`, the refusal is an `invalid` tool error that names the field,
/// its size and the limit; nothing is registered; and the schema states each limit.
#[tokio::test]
async fn resource_size_limits_are_refused_as_invalid_and_described() {
    let client = in_process(Store::open_in_memory(Clock::System).unwrap()).await;
    call_ok(
        &client,
        "register_agent",
        json!({ "name": "alice", "program": "claude" }),
    )
    .await;
    let cases = [
        (
            json!({ "endpoint": "x".repeat(another_core::MAX_RESOURCE_ENDPOINT_BYTES + 1) }),
            "resource endpoint is 2049 bytes; the limit is 2048",
        ),
        (
            json!({ "purpose": "x".repeat(another_core::MAX_RESOURCE_PURPOSE_BYTES + 1) }),
            "resource purpose is 1025 bytes; the limit is 1024",
        ),
        (
            json!({ "metadata": { "blob": "x".repeat(another_core::MAX_RESOURCE_METADATA_BYTES) } }),
            "metadata is 65547 bytes as JSON; the limit is 65536",
        ),
    ];
    for (extra, message) in cases {
        let mut args = json!({ "agent": "alice", "kind": "model_server", "name": "ollama" });
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let error = call_error(&client, "resource_register", args).await;
        assert_eq!(error["code"], "invalid", "{error}");
        assert_eq!(error["message"], message);
        assert!(error.get("hint").is_none(), "{error}");
    }
    let found = call_ok(&client, "resource_find", json!({})).await;
    assert_eq!(
        found["resources"],
        json!([]),
        "a refusal registered a resource"
    );

    let tools = client.list_all_tools().await.unwrap();
    let schema = tools
        .iter()
        .find(|t| t.name == "resource_register")
        .unwrap();
    let schema = serde_json::to_value(&schema.input_schema).unwrap();
    for (field, limit) in [
        ("endpoint", "At most 2 KiB"),
        ("purpose", "At most 1 KiB"),
        ("metadata", "At most 64 KiB"),
    ] {
        let described = schema["properties"][field]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(described.contains(limit), "{field}: {described:?}");
    }
    client.cancel().await.unwrap();
}

/// Q19b review R1: a description, model, hold reason or resource text that is not one line
/// is refused (D97). The error names the field and what it found, and its hint is about
/// that, not about how to pass an agent name. The tool schemas say so before a model sends one.
#[tokio::test]
async fn one_line_fields_are_described_and_their_refusal_has_a_fitting_hint() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("site");
    std::fs::create_dir_all(&project).unwrap();
    let project = project.to_string_lossy().into_owned();
    let client = in_process(Store::open_in_memory(Clock::System).unwrap()).await;
    call_ok(
        &client,
        "register_agent",
        json!({ "name": "alice", "program": "claude", "project": project }),
    )
    .await;

    let two_lines = "Working on X\nand Y";
    let reversed = "fine\u{202e}gpj.rs";
    let register = |extra: Value| {
        let mut args = json!({ "name": "alice", "program": "claude", "project": project });
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        args
    };
    let resource = |extra: Value| {
        let mut args = json!({ "agent": "alice", "kind": "model_server", "name": "ollama" });
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        args
    };
    let hold = |reason: &str| json!({ "agent": "alice", "project": project, "paths": ["src/a.rs"], "reason": reason });
    let cases = [
        (
            "register_agent",
            register(json!({ "description": two_lines })),
            "an agent description",
            "control characters",
        ),
        (
            "register_agent",
            register(json!({ "description": reversed })),
            "an agent description",
            "text-direction",
        ),
        (
            "register_agent",
            register(json!({ "model": two_lines })),
            "an agent model",
            "control characters",
        ),
        (
            "hold_paths",
            hold(two_lines),
            "a hold reason",
            "control characters",
        ),
        (
            "hold_paths",
            hold(reversed),
            "a hold reason",
            "text-direction",
        ),
        (
            "resource_register",
            resource(json!({ "endpoint": two_lines })),
            "a resource endpoint",
            "control characters",
        ),
        (
            "resource_register",
            resource(json!({ "purpose": reversed })),
            "a resource purpose",
            "text-direction",
        ),
        (
            "resource_register",
            resource(json!({ "name": reversed })),
            "a resource name",
            "text-direction",
        ),
    ];
    for (tool, args, field, found) in cases {
        let error = call_error(&client, tool, args).await;
        assert_eq!(error["code"], "invalid", "{tool} {field}: {error}");
        let message = error["message"].as_str().unwrap();
        assert!(message.starts_with(field), "{tool}: {message}");
        assert!(message.contains(found), "{tool} {field}: {message}");
        let hint = error["hint"].as_str().unwrap_or_default();
        assert!(hint.contains("one line"), "{tool} {field}: hint {hint:?}");
        assert!(
            !hint.contains("name@project"),
            "{tool} {field}: hint {hint:?}"
        );
    }

    let tools = client.list_all_tools().await.unwrap();
    for (tool, fields) in [
        ("register_agent", &["model", "description"][..]),
        ("hold_paths", &["reason"][..]),
        ("resource_register", &["name", "endpoint", "purpose"][..]),
    ] {
        let schema = tools.iter().find(|t| t.name == tool).unwrap();
        let schema = serde_json::to_value(&schema.input_schema).unwrap();
        for field in fields {
            let described = schema["properties"][field]["description"]
                .as_str()
                .unwrap_or_default();
            assert!(
                described.contains("One line"),
                "{tool}.{field} does not say it is one line: {described:?}"
            );
        }
    }
    client.cancel().await.unwrap();
}
