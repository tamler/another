use std::io::Write;
use std::process::{Command, Output, Stdio};

use rusqlite::Connection;
use serde_json::{json, Value};

#[test]
fn hook_environment_pair_requires_explicit_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("shop");
    let home = dir.path().join("home");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let run = |event: &str, agent: Option<&str>, mode: Option<&str>| -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_another"));
        cmd.args(["hook", "run", event, "--client", "claude"])
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("ANOTHER_HOME", home.join(".another"))
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_AGENT_MODE")
            .env_remove("GROK_HOOK_EVENT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(agent) = agent {
            cmd.env("ANOTHER_AGENT", agent);
        }
        if let Some(mode) = mode {
            cmd.env("ANOTHER_AGENT_MODE", mode);
        }
        let mut child = cmd.spawn().unwrap();
        let input = json!({"session_id":"matrix-session", "cwd":root,
            "hook_event_name":"PreToolUse", "tool_name":"Edit",
            "tool_input":{"file_path":root.join("src/a.rs")}});
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    // Hooks never create a home: it exists before any session starts.
    assert!(Command::new(env!("CARGO_BIN_EXE_another"))
        .arg("init")
        .env("ANOTHER_HOME", home.join(".another"))
        .output()
        .unwrap()
        .status
        .success());
    let start = run("session-start", None, None);
    assert!(start.status.success());
    let text = String::from_utf8(start.stdout).unwrap();
    let actor = text
        .strip_prefix("Another: ")
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();
    let db = Connection::open(home.join(".another/another.db")).unwrap();
    let snapshot = || -> Vec<i64> {
        ["projects", "agents", "holds", "deliveries", "events"]
            .iter()
            .map(|table| {
                db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                    .unwrap()
            })
            .chain(std::iter::once(
                db.query_row(
                    "SELECT count(*) FROM events WHERE kind='session.binding.v1'",
                    [],
                    |r| r.get(0),
                )
                .unwrap(),
            ))
            .collect()
    };
    for (agent, mode) in [
        (None, Some("base")),
        (None, Some("session")),
        (None, Some("")),
        (Some(""), None),
        (Some("  "), Some("base")),
        (Some("x"), None),
        (Some("x"), Some("")),
        (Some("x"), Some("unknown")),
        (Some("x"), Some("session")),
        (Some("x@@bad"), Some("session")),
    ] {
        let before = snapshot();
        let denied_start = run("session-start", agent, mode);
        assert!(denied_start.status.success());
        let expected = if mode == Some("session") && agent.is_some_and(|a| !a.trim().is_empty()) {
            "binding"
        } else {
            "ANOTHER_AGENT_MODE"
        };
        assert!(
            String::from_utf8_lossy(&denied_start.stdout).contains(expected),
            "start diagnostic missing: {agent:?} {mode:?}"
        );
        assert_eq!(snapshot(), before, "start mutated: {agent:?} {mode:?}");
        let denied = run("pre-edit", agent, mode);
        let response: Value = serde_json::from_slice(&denied.stdout).unwrap();
        assert_eq!(response["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(snapshot(), before, "pre-edit mutated: {agent:?} {mode:?}");
    }
    let session_start = run("session-start", Some(actor), Some("session"));
    assert!(String::from_utf8_lossy(&session_start.stdout).contains(actor));
    let base_start = run("session-start", Some(actor), Some("base"));
    assert!(base_start.status.success());
    assert!(String::from_utf8_lossy(&base_start.stdout).starts_with("Another: "));

    let output = dir.path().join("session output ü.txt");
    let child = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["session", "exec", "--agent", actor, "--"])
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", "session_exec_probe"])
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("ANOTHER_HOME", home.join(".another"))
        .env("ANOTHER_AGENT", "wrong")
        .env("ANOTHER_AGENT_MODE", "base")
        .env("PROVENANCE_OUTPUT", &output)
        .output()
        .unwrap();
    assert!(child.status.success(), "session exec failed: {child:?}");
    let inherited: Value = serde_json::from_slice(&std::fs::read(output).unwrap()).unwrap();
    assert_eq!(inherited["agent"], actor);
    assert_eq!(inherited["mode"], "session");
}

#[test]
fn session_exec_probe() {
    if let Some(path) = std::env::var_os("PROVENANCE_OUTPUT") {
        std::fs::write(
            path,
            json!({"agent": std::env::var("ANOTHER_AGENT").unwrap(),
            "mode": std::env::var("ANOTHER_AGENT_MODE").unwrap()})
            .to_string(),
        )
        .unwrap();
    }
}

#[cfg(unix)]
#[test]
fn installed_wrapper_exposes_session_start_identity_diagnostic() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let root = dir.path().join("project");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(root.join("file.rs"), "before").unwrap();
    let root = std::fs::canonicalize(root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let mut install = Command::new(env!("CARGO_BIN_EXE_another"));
    let result = install
        .args(["hook", "install", "--client", "claude"])
        .env("HOME", &home)
        .env("ANOTHER_HOME", home.join(".another"))
        .env_remove("ANOTHER_AGENT")
        .env_remove("ANOTHER_AGENT_MODE")
        .output()
        .unwrap();
    assert!(result.status.success());
    let mut child = Command::new(home.join(".another/hooks/another-hook.sh"))
        .args(["session-start", "--client", "claude"])
        .env("HOME", &home)
        .env("ANOTHER_HOME", home.join(".another"))
        .env("ANOTHER_AGENT", "custom")
        .env_remove("ANOTHER_AGENT_MODE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            json!({"session_id":"wrapper-test", "cwd":root})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ANOTHER_AGENT_MODE"),
        "diagnostic lost: {output:?}"
    );
    for (agent, mode) in [
        (None, Some("base")),
        (None, Some("session")),
        (None, Some("")),
        (Some(""), Some("base")),
        (Some(" "), Some("session")),
        (Some("private-agent"), None),
        (Some("private-agent"), Some("")),
        (Some("private-agent"), Some("wrong")),
    ] {
        for event in ["session-start", "pre-edit", "pre-controlled-edit"] {
            let mut cmd = Command::new(home.join(".another/hooks/another-hook.sh"));
            cmd.args([event, "--client", "claude"])
                .env("HOME", &home)
                .env("ANOTHER_HOME", home.join(".another"))
                .env_remove("ANOTHER_AGENT")
                .env_remove("ANOTHER_AGENT_MODE")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if let Some(value) = agent {
                cmd.env("ANOTHER_AGENT", value);
            }
            if let Some(value) = mode {
                cmd.env("ANOTHER_AGENT_MODE", value);
            }
            let mut child = cmd.spawn().unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(
                    json!({"session_id":"private-id", "cwd":root})
                        .to_string()
                        .as_bytes(),
                )
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success());
            let stdout = String::from_utf8(output.stdout).unwrap();
            let message = if event != "session-start" {
                let response: Value = serde_json::from_str(&stdout).unwrap();
                assert_eq!(response["hookSpecificOutput"]["permissionDecision"], "deny");
                response["hookSpecificOutput"]["permissionDecisionReason"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            } else {
                stdout
            };
            if event != "pre-edit" {
                assert!(
                    message.contains("nonempty ANOTHER_AGENT")
                        && message.contains("ANOTHER_AGENT_MODE"),
                    "incorrect advice: {message}"
                );
            }
            assert!(!message.contains("private-agent") && !message.contains("private-id"));
            assert!(output.stderr.is_empty());
        }
    }
    let wrapper = |event: &str, input: Value, agent: Option<&str>| -> String {
        let mut command = Command::new(home.join(".another/hooks/another-hook.sh"));
        command
            .args([event, "--client", "claude"])
            .env("HOME", &home)
            .env("ANOTHER_HOME", home.join(".another"))
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_AGENT_MODE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        if let Some(agent) = agent {
            command
                .env("ANOTHER_AGENT", agent)
                .env("ANOTHER_AGENT_MODE", "session");
        }
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        let result = child.wait_with_output().unwrap();
        assert!(result.status.success());
        String::from_utf8(result.stdout).unwrap()
    };
    let valid = wrapper(
        "session-start",
        json!({"session_id":"private-original", "cwd":root}),
        None,
    );
    let actor = valid
        .strip_prefix("Another: ")
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();
    for event in ["session-start", "pre-controlled-edit"] {
        for input in [
            json!({"cwd":root}),
            json!({"cwd":root, "session_id":""}),
            json!({"cwd":root, "session_id":"private-one", "sessionId":"private-two"}),
        ] {
            let result = wrapper(event, input, None);
            assert!(
                result.contains("client session ID"),
                "wrong client-ID diagnostic: {result}"
            );
            assert!(!result.contains("private-one") && !result.contains("private-two"));
        }
        let result = wrapper(
            event,
            json!({"cwd":root, "session_id":"private-other","tool_use_id":"provenance-call","tool_name":"mcp__another__controlled_edit",
                "tool_input":{"project":root,"path":"file.rs","expected_sha256":another_core::controlled_sha256(b"before"),"replacement":"after"}}),
            Some(actor),
        );
        assert!(
            result.contains("binding") || result.contains("session actor"),
            "wrong binding diagnostic: {result}"
        );
        assert!(!result.contains("private-other"));
    }
    for (event, client) in [
        ("prompt", "claude"),
        ("post-tool", "claude"),
        ("session-start", "cursor"),
        ("post-tool", "cursor"),
    ] {
        let mut child = Command::new(home.join(".another/hooks/another-hook.sh"))
            .args([event, "--client", client])
            .env("HOME", &home)
            .env("ANOTHER_HOME", home.join(".another"))
            .env("ANOTHER_AGENT", "custom")
            .env_remove("ANOTHER_AGENT_MODE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                json!({"session_id":"wrapper-test", "cwd":root})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        let text = String::from_utf8(out.stdout).unwrap();
        if client == "cursor" || event == "post-tool" {
            let response: Value = serde_json::from_str(&text).unwrap();
            let context = if client == "cursor" {
                &response["additional_context"]
            } else {
                &response["hookSpecificOutput"]["additionalContext"]
            };
            assert!(context.as_str().unwrap().contains("ANOTHER_AGENT_MODE"));
        } else {
            assert!(text.contains("ANOTHER_AGENT_MODE"));
        }
    }
}
