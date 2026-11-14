use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn native_edit_is_denied_before_it_can_reopen_the_checked_path() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let project = dir.path().join("project");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    let command = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_another"));
        command
            .args(args)
            .current_dir(&project)
            .env("ANOTHER_HOME", &home)
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_AGENT_MODE");
        command
    };
    assert!(command(&["init"]).output().unwrap().status.success());
    let invoke = |event: &str, input: serde_json::Value| {
        let mut child = command(&["hook", "run", event, "--client", "claude"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap()
    };
    let start = invoke(
        "session-start",
        serde_json::json!({"session_id":"session-a", "cwd":project}),
    );
    assert!(
        start.starts_with("Another: "),
        "session start did not succeed: {start}"
    );
    let result = invoke(
        "pre-edit",
        serde_json::json!({
            "session_id":"session-a", "cwd":project, "tool_use_id":"call-a",
            "hook_event_name":"PreToolUse", "tool_name":"Edit",
            "tool_input":{"file_path":project.join("a.rs")}
        }),
    );
    let result: serde_json::Value =
        serde_json::from_str(&result).expect("native edit must receive a denial");
    assert_eq!(result["hookSpecificOutput"]["permissionDecision"], "deny");
}
