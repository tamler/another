//! Synthetic stdio protocol traffic through the real executable, including debug logs.
use another_core::{controlled_sha256, Store};
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

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
