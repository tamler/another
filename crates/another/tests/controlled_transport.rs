//! Synthetic stdio protocol traffic through the real executable, including debug logs.
use another_core::{controlled_sha256, Store};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn stdio_call(
    home: &std::path::Path,
    client: &str,
    version: &str,
    args: serde_json::Value,
) -> serde_json::Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["serve", "--stdio"])
        .env("ANOTHER_HOME", home)
        .env_remove("ANOTHER_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    writeln!(input,"{}",json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":client,"version":version}}})).unwrap();
    let mut line = String::new();
    assert!(output.read_line(&mut line).unwrap() > 0);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&line).unwrap()["id"],
        1
    );
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":args})
    )
    .unwrap();
    let response = loop {
        line.clear();
        assert!(
            output.read_line(&mut line).unwrap() > 0,
            "stdio ended before tool response"
        );
        let response: serde_json::Value = serde_json::from_str(&line).unwrap();
        if response["id"] == 2 {
            break response;
        }
    };
    drop(input);
    drop(output);
    assert!(child.wait_with_output().unwrap().status.success());
    response
}

#[test]
fn hook_cli_then_real_stdio_confirms_exactly_once() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let root = temp.path().join("project");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let root = std::fs::canonicalize(root).unwrap();
    std::fs::write(root.join("a.rs"), b"before").unwrap();
    drop(Store::open(&home).unwrap());
    let edit = json!({"project":root,"path":"a.rs","expected_sha256":controlled_sha256(b"before"),"replacement":"after"});
    for event in ["session-start", "pre-controlled-edit"] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_another"))
            .args(["hook", "run", event, "--client", "claude"])
            .env("ANOTHER_HOME", &home)
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_AGENT_MODE")
            .env_remove("GROK_HOOK_EVENT")
            .env_remove("HERDR_PANE_ID")
            .env_remove("TMUX_PANE")
            .env_remove("CMUX_SURFACE_ID")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(child.stdin.take().unwrap(),"{}",json!({"session_id":"fixture-session","cwd":root,"tool_name":"mcp__another__controlled_edit","tool_use_id":"fixture-call","tool_input":edit})).unwrap();
        let result = child.wait_with_output().unwrap();
        assert!(result.status.success());
        if event == "pre-controlled-edit" {
            assert!(result.stdout.is_empty(), "admission was refused");
        }
    }
    let params = json!({"name":"controlled_edit","_meta":{"claudecode/toolUseId":"fixture-call"},"arguments":edit});
    let response = stdio_call(&home, "claude-code", "2.1.325", params.clone());
    assert!(
        response.get("error").is_none(),
        "controlled call returned protocol error"
    );
    assert_ne!(response["result"]["isError"], true);
    assert!(response["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .starts_with("confirmed "));
    assert_eq!(std::fs::read(root.join("a.rs")).unwrap(), b"after");
    let replay = stdio_call(&home, "claude-code", "2.1.325", params);
    assert_eq!(replay["result"]["isError"], true);
    assert_eq!(std::fs::read(root.join("a.rs")).unwrap(), b"after");
}

#[test]
fn unobserved_codex_client_identity_is_unsupported() {
    let temp = tempfile::tempdir().unwrap();
    drop(Store::open(temp.path()).unwrap());
    let response = stdio_call(
        temp.path(),
        "codex-mcp-client",
        "0.169.0",
        json!({"name":"controlled_edit","arguments":{}}),
    );
    assert_eq!(
        response["error"]["message"],
        "unsupported controlled client version"
    );
}

#[test]
fn debug_logging_does_not_expose_controlled_call_metadata_or_contents() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let root = temp.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let root = std::fs::canonicalize(root).unwrap();
    std::fs::write(root.join("a.rs"), b"before").unwrap();
    let store = Store::open(&home).unwrap();
    store
        .ensure_project("fixture", root.to_str().unwrap())
        .unwrap();
    drop(store);
    let mut child = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["serve", "--stdio"])
        .env("ANOTHER_HOME", &home)
        .env("ANOTHER_LOG", "debug,rmcp::service=trace")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    writeln!(input,"{}",json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"claude-code","version":"2.1.325"}}})).unwrap();
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&line).unwrap()["id"],
        1
    );
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    writeln!(input,"{}",json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"controlled_edit","_meta":{"claudecode/toolUseId":"PRIVATE-CALL-SENTINEL"},"arguments":{"project":root,"path":"a.rs","expected_sha256":controlled_sha256(b"before"),"replacement":"PRIVATE-CONTENT-SENTINEL"}}})).unwrap();
    line.clear();
    loop {
        assert!(
            output.read_line(&mut line).unwrap() > 0,
            "stdio ended before tool response"
        );
        let response: serde_json::Value = serde_json::from_str(&line).unwrap();
        if response["id"] == 2 {
            assert_eq!(response["result"]["isError"], true);
            break;
        }
        line.clear();
    }
    drop(input);
    drop(output);
    let finished = child.wait_with_output().unwrap();
    let logs = String::from_utf8(finished.stderr).unwrap();
    assert!(
        !logs.contains("PRIVATE-CALL-SENTINEL"),
        "raw call metadata reached logs"
    );
    assert!(
        !logs.contains("PRIVATE-CONTENT-SENTINEL"),
        "file contents reached logs"
    );
    assert_eq!(std::fs::read(root.join("a.rs")).unwrap(), b"before");
}
