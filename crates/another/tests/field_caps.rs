//! Field size rules on the local paths (Q26, D99): an agent's model and description, a
//! hold's reason and pattern, and a request's closing note. The CLI prints one line that
//! names the field, its size and the limit and exits non-zero; the MCP tools, which
//! `another serve` also runs for remote callers, answer `invalid` with the same text.
//! Nothing is written either way.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use another::mcp::AnotherMcp;
use another_core::*;
use rmcp::model::{CallToolRequestParams, ClientConfig};
use rmcp::ServiceExt;
use serde_json::{json, Value};

fn another(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_another"))
        .args(args)
        .env("ANOTHER_HOME", home)
        .env_remove("ANOTHER_AGENT")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn ok(home: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend_from_slice(args);
    let out = another(home, &all);
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// The five refusals, each one byte over its limit, in the order the tests give them.
const REFUSALS: [&str; 5] = [
    "agent model is 201 bytes; the limit is 200",
    "agent description is 4097 bytes; the limit is 4096",
    "hold reason is 1025 bytes; the limit is 1024",
    "hold pattern is 4097 bytes; the limit is 4096",
    "request note is 16385 bytes; the limit is 16384",
];

fn over(max: usize) -> String {
    "x".repeat(max + 1)
}

#[test]
fn the_cli_refuses_each_field_over_its_limit_with_one_line() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("shop");
    std::fs::create_dir_all(&repo).unwrap();
    let repo = repo.to_str().unwrap();
    for name in ["alice", "bob"] {
        ok(
            &home,
            &[
                "agent",
                "register",
                name,
                "--program",
                "claude",
                "--project",
                repo,
            ],
        );
    }
    let sent = ok(
        &home,
        &[
            "send",
            "--from",
            "alice",
            "--to",
            "bob",
            "--subject",
            "Review",
            "--body",
            "please",
            "--due",
            "3600",
        ],
    );
    let rid = sent["request_id"].as_str().unwrap();
    let store = Store::open(&home).unwrap();
    let before = store.events(10_000).unwrap().len();
    drop(store);

    let (model, description, reason, pattern, note) = (
        over(MAX_AGENT_MODEL_BYTES),
        over(MAX_AGENT_DESCRIPTION_BYTES),
        over(MAX_HOLD_REASON_BYTES),
        over(MAX_HOLD_PATTERN_BYTES),
        over(MAX_REQUEST_NOTE_BYTES),
    );
    let register = ["agent", "register", "dave", "--program", "claude"];
    let hold = ["hold", "acquire", "--agent", "alice", "--project", repo];
    let cases: [Vec<&str>; 5] = [
        [&register[..], &["--model", &model]].concat(),
        [&register[..], &["--description", &description]].concat(),
        [&hold[..], &["src/a.rs", "--reason", &reason]].concat(),
        [&hold[..], &[&pattern]].concat(),
        vec![
            "request", "complete", rid, "--agent", "bob", "--note", &note,
        ],
    ];
    for (args, refusal) in cases.iter().zip(REFUSALS) {
        let out = another(&home, args);
        assert!(!out.status.success(), "{refusal}");
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            format!("another: {refusal}\n")
        );
        assert!(out.stdout.is_empty(), "{refusal}");
    }
    let store = Store::open(&home).unwrap();
    assert_eq!(store.events(10_000).unwrap().len(), before);
}

#[tokio::test]
async fn the_mcp_tools_refuse_each_field_over_its_limit_as_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("shop");
    std::fs::create_dir_all(&repo).unwrap();
    let repo = repo.to_str().unwrap().to_string();
    let store = Store::open(&dir.path().join("home")).unwrap();
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let served = store.clone();
    tokio::spawn(async move {
        let server = AnotherMcp::new(served).serve(server_io).await.unwrap();
        let _ = server.waiting().await;
    });
    let client = ClientConfig::default().serve(client_io).await.unwrap();
    let call = |tool: &'static str, args: Value| {
        let client = &client;
        async move {
            let res = client
                .call_tool(
                    CallToolRequestParams::new(tool.to_string())
                        .with_arguments(args.as_object().cloned().unwrap()),
                )
                .await
                .unwrap();
            let failed = res.is_error == Some(true);
            let body = match serde_json::to_value(&res.content).unwrap()[0]["text"].as_str() {
                Some(text) if failed => serde_json::from_str(text).unwrap(),
                _ => res.structured_content.unwrap_or(Value::Null),
            };
            (failed, body)
        }
    };
    for name in ["alice", "bob"] {
        let (failed, body) = call(
            "register_agent",
            json!({ "name": name, "program": "claude", "project": repo }),
        )
        .await;
        assert!(!failed, "{body}");
    }
    let (failed, sent) = call(
        "send_message",
        json!({ "from": "alice", "to": ["bob"], "subject": "Review", "body": "please",
                "create_request": true, "due_in_seconds": 3600 }),
    )
    .await;
    assert!(!failed, "{sent}");
    let rid = sent["request_id"].as_str().unwrap().to_string();
    let before = store.events(10_000).unwrap().len();

    let register = |field: &str, value: String| {
        let mut args = json!({ "name": "dave", "program": "claude" });
        args[field] = json!(value);
        args
    };
    let hold = |path: String, reason: Option<String>| json!({ "agent": "alice", "project": repo, "paths": [path], "reason": reason });
    let cases = [
        (
            "register_agent",
            register("model", over(MAX_AGENT_MODEL_BYTES)),
        ),
        (
            "register_agent",
            register("description", over(MAX_AGENT_DESCRIPTION_BYTES)),
        ),
        (
            "hold_paths",
            hold("src/a.rs".into(), Some(over(MAX_HOLD_REASON_BYTES))),
        ),
        ("hold_paths", hold(over(MAX_HOLD_PATTERN_BYTES), None)),
        (
            "claim_request",
            json!({ "agent": "bob", "request_id": rid, "action": "complete",
                    "note": over(MAX_REQUEST_NOTE_BYTES) }),
        ),
    ];
    for ((tool, args), refusal) in cases.into_iter().zip(REFUSALS) {
        let (failed, body) = call(tool, args).await;
        assert!(failed, "{refusal}: {body}");
        assert_eq!(body["error"]["code"], "invalid", "{refusal}");
        assert_eq!(body["error"]["message"], refusal);
    }
    assert_eq!(store.events(10_000).unwrap().len(), before);
}

/// The reason the edit hook gives for its holds is a small fraction of the limit, and the
/// hook's holds are not held to the field rules at all (`Store::guard_edit`), so a long
/// file path cannot make an edit fail.
#[test]
fn the_edit_hooks_own_reason_is_far_under_the_limit() {
    assert!(another::hooks::AUTO_HOLD_REASON.len() * 10 < MAX_HOLD_REASON_BYTES);
}
