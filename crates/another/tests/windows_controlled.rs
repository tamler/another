//! Native Windows launcher and handle route. Cross-compiling this is not execution.
#![cfg(windows)]

use another_core::{controlled_sha256, ControlledEdit, Store};
use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn powershell_hooks_admit_only_the_controlled_existing_file() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("profile");
    let data = home.join(".another");
    let root = temp.path().join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join("a.rs"), b"before").unwrap();
    let root = std::fs::canonicalize(root).unwrap();
    let mut install = Command::new(env!("CARGO_BIN_EXE_another"));
    let installed = install
        .args([
            "hook",
            "install",
            "--client",
            "claude",
            "--strict-controlled",
        ])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("ANOTHER_HOME", &data)
        .output()
        .unwrap();
    assert!(installed.status.success(), "native hook install failed");
    let config: Value =
        serde_json::from_slice(&std::fs::read(home.join(".claude/settings.json")).unwrap())
            .unwrap();
    let run = |command: &str, input: Value| {
        let encoded = command.split_once("-EncodedCommand ").unwrap().1;
        let mut child = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-EncodedCommand",
                encoded,
            ])
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("ANOTHER_HOME", &data)
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_AGENT_MODE")
            .env_remove("GROK_HOOK_EVENT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        let result = child.wait_with_output().unwrap();
        assert!(result.status.success(), "native hook process failed");
        String::from_utf8(result.stdout).unwrap()
    };
    let start = config["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(
        run(start, json!({"session_id":"windows-session","cwd":root})).starts_with("Another: ")
    );
    let groups = config["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(
        groups.len(),
        2,
        "native denial and controlled admission must both remain installed"
    );
    let native = groups
        .iter()
        .find(|g| g["matcher"] == "Edit|Write|MultiEdit|NotebookEdit")
        .unwrap()["hooks"][0]["command"]
        .as_str()
        .unwrap();
    let denied: Value = serde_json::from_str(&run(native, json!({}))).unwrap();
    assert_eq!(denied["hookSpecificOutput"]["permissionDecision"], "deny");
    let controlled = groups
        .iter()
        .find(|g| g["matcher"] == "^mcp__another__controlled_edit$")
        .unwrap()["hooks"][0]["command"]
        .as_str()
        .unwrap();
    let edit = ControlledEdit {
        project: root.to_str().unwrap().into(),
        path: "a.rs".into(),
        expected_sha256: controlled_sha256(b"before"),
        replacement: "after".into(),
    };
    assert!(run(controlled,json!({"session_id":"windows-session","cwd":root,"tool_name":"mcp__another__controlled_edit","tool_use_id":"windows-call","tool_input":edit})).trim().is_empty());
    let store = Store::open(&data).unwrap();
    let result = another::controlled_edit::execute(&store, "claude", "windows-call", &edit);
    assert!(result.is_ok(), "native controlled edit failed");
    assert_eq!(std::fs::read(root.join("a.rs")).unwrap(), b"after");
    assert!(another::controlled_edit::execute(&store, "claude", "windows-call", &edit).is_err());
    drop(store);
    let restored = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["hook", "install", "--client", "claude"])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("ANOTHER_HOME", &data)
        .output()
        .unwrap();
    assert!(restored.status.success());
    let advisory: Value =
        serde_json::from_slice(&std::fs::read(home.join(".claude/settings.json")).unwrap())
            .unwrap();
    assert_eq!(advisory["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
    let command = advisory["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(run(command, json!({"session_id":"windows-session","cwd":root,"tool_name":"Write","tool_input":{"file_path":root.join("new.rs")}})).trim().is_empty());
    let status = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["--json", "hook", "status"])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("ANOTHER_HOME", &data)
        .output()
        .unwrap();
    assert!(status.status.success());
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(
        status
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["client"] == "claude")
            .unwrap()["installed"],
        true
    );
}
