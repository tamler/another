//! `send_message` over MCP with `reply_to` but no `subject`, when it also names recipients
//! (or opens a request), derives the subject from the parent: "Re: " once, however many the
//! parent had, and one safe line even when the parent's subject was stored before subjects
//! were checked (D75). With neither a subject nor `reply_to`, it is refused.

use another::mcp::AnotherMcp;
use another_core::{Program, RegisterAgent, Store, DB_FILE};
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};

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

async fn call_err(client: &Client, tool: &str, args: Value) -> (String, String) {
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

async fn client(store: Store) -> Client {
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let server = AnotherMcp::new(store).serve(server_io).await.unwrap();
        let _ = server.waiting().await;
    });
    ClientConfig::default().serve(client_io).await.unwrap()
}

fn store(dir: &std::path::Path) -> Store {
    let s = Store::open(&dir.join("home")).unwrap();
    for name in ["lead", "worker", "other"] {
        s.register_agent(RegisterAgent {
            name: name.into(),
            program: Program::Claude,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    }
    s
}

#[tokio::test]
async fn a_reply_with_recipients_takes_one_re_prefix_from_its_parent() {
    let dir = tempfile::tempdir().unwrap();
    let c = client(store(dir.path())).await;
    let parent = call_ok(
        &c,
        "send_message",
        json!({ "from": "lead", "to": ["worker"], "subject": "Re: Re: Plan", "body": "b" }),
    )
    .await;
    let reply = call_ok(
        &c,
        "send_message",
        json!({ "from": "worker", "to": ["lead", "other"], "reply_to": parent["message_id"],
                "body": "looping in other" }),
    )
    .await;
    assert_eq!(reply["subject"], "Re: Plan");
    assert_eq!(reply["thread_id"], parent["thread_id"]);
    assert_eq!(reply["reply_to"], parent["message_id"]);
    assert_eq!(reply["to"], json!(["lead", "other"]));
}

#[tokio::test]
async fn a_legacy_parent_subject_becomes_one_safe_line() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let parent = s
        .send_message(another_core::SendMessage {
            from: "lead".into(),
            to: vec!["worker".into()],
            subject: "placeholder".into(),
            body: "b".into(),
            ..Default::default()
        })
        .unwrap();
    // A subject stored before D75 refused control characters.
    rusqlite::Connection::open(dir.path().join("home").join(DB_FILE))
        .unwrap()
        .execute(
            "UPDATE messages SET subject = ?2 WHERE message_id = ?1",
            rusqlite::params![parent.message_id, "Re: deploy\n\u{1b}[31mnow\u{202e}"],
        )
        .unwrap();
    let c = client(s).await;
    let reply = call_ok(
        &c,
        "send_message",
        json!({ "from": "worker", "to": ["lead"], "reply_to": parent.message_id, "body": "ok" }),
    )
    .await;
    let subject = reply["subject"].as_str().unwrap();
    assert!(subject.starts_with("Re: deploy"), "{subject:?}");
    assert!(
        !subject.chars().any(another_core::is_unsafe_text_char),
        "{subject:?}"
    );
}

#[tokio::test]
async fn a_new_message_needs_a_subject() {
    let dir = tempfile::tempdir().unwrap();
    let c = client(store(dir.path())).await;
    let (code, message) = call_err(
        &c,
        "send_message",
        json!({ "from": "lead", "to": ["worker"], "body": "b" }),
    )
    .await;
    assert_eq!(code, "invalid", "{message}");
    assert!(message.contains("subject is required"), "{message}");
}
