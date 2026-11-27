//! CLI errors say what happened and what to do next, and exit with a status that names
//! the class of failure: 0 success, 1 error, 2 bad arguments, 3 conflict.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::Value;

fn another(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    {
        let mut legacy_fixture_command = Command::new(env!("CARGO_BIN_EXE_another"));
        legacy_fixture_command.arg("--unguarded-repair");
        legacy_fixture_command
    }
    .args(args)
    .current_dir(cwd)
    .env("ANOTHER_HOME", home)
    .env_remove("ANOTHER_AGENT")
    .env_remove("ANOTHER_LOG")
    .stdin(Stdio::null())
    .output()
    .expect("run another")
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn heartbeat_rejects_start_and_end_before_session_mutation() {
    let fx = Fx::new();
    fx.ok(&["heartbeat", "--agent", "lead", "--start"]);
    let store = another_core::Store::open(&fx.home).unwrap();
    let before = store.agent("lead").unwrap();
    let (code, message) = fx.fail(&["heartbeat", "--agent", "lead", "--start", "--end"]);
    assert_eq!(code, Some(2));
    assert!(
        message.contains("--start") && message.contains("--end"),
        "{message}"
    );
    let after = store.agent("lead").unwrap();
    assert_eq!(after.status, before.status);
    assert_eq!(after.last_seen_at, before.last_seen_at);
}

struct Fx {
    _dir: tempfile::TempDir,
    home: std::path::PathBuf,
    shop: std::path::PathBuf,
}

impl Fx {
    fn new() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let shop = dir.path().join("shop");
        std::fs::create_dir_all(&shop).unwrap();
        let fx = Fx {
            _dir: dir,
            home,
            shop,
        };
        fx.ok(&["init"]);
        for (name, program) in [("lead", "claude"), ("worker", "codex")] {
            fx.ok(&[
                "agent",
                "register",
                name,
                "--program",
                program,
                "--project",
                ".",
            ]);
        }
        fx
    }

    fn run(&self, args: &[&str]) -> Output {
        another(&self.home, &self.shop, args)
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        text(&out.stdout)
    }

    /// Run a failing command; return (exit code, stderr).
    fn fail(&self, args: &[&str]) -> (Option<i32>, String) {
        let out = self.run(args);
        assert!(
            !out.status.success(),
            "{args:?} succeeded: {}",
            text(&out.stdout)
        );
        (out.status.code(), text(&out.stderr))
    }

    /// Run a failing command with --json; return (exit code, the error object).
    fn fail_json(&self, args: &[&str]) -> (Option<i32>, Value) {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let out = self.run(&all);
        assert!(!out.status.success());
        let v: Value = serde_json::from_slice(&out.stdout).expect("json error on stdout");
        (out.status.code(), v["error"].clone())
    }
}

/// Every "unknown X" a user can type names the command that lists the real ones.
#[test]
fn unknown_ids_and_names_say_where_to_find_the_real_ones() {
    let fx = Fx::new();
    let cases: &[(&[&str], &str, &str)] = &[
        (
            &["inbox", "--agent", "nobody"],
            "unknown agent \"nobody\"",
            "another agent list",
        ),
        (
            &["request", "claim", "--agent", "worker", "01NOSUCHREQUEST"],
            "unknown request",
            "another requests",
        ),
        (
            &["reply", "--from", "lead", "01NOSUCHMESSAGE", "--body", "x"],
            "unknown message",
            "another inbox",
        ),
        (
            &["thread", "01NOSUCHTHREAD"],
            "unknown thread",
            "another threads",
        ),
        (
            // Shaped like a hold id (26 characters), so it is read as one and not as a path.
            &[
                "hold",
                "release",
                "--agent",
                "worker",
                "01N0SVCHH01D0000000000000X",
            ],
            "no live hold",
            "another holds",
        ),
        (
            &["borrow", "--agent", "lead", "nosuchresource"],
            "no registered resource",
            "another resources",
        ),
        (
            &["token", "revoke", "nosuchtoken"],
            "unknown token",
            "another token list",
        ),
    ];
    for (args, what, hint) in cases {
        let (code, err) = fx.fail(args);
        assert_eq!(code, Some(1), "{args:?}: {err}");
        assert!(err.contains(what), "{args:?}: {err}");
        let hint_line = err
            .lines()
            .find(|l| l.starts_with("hint: "))
            .unwrap_or_else(|| panic!("{args:?} has no hint line: {err}"));
        assert!(hint_line.contains(hint), "{args:?}: {hint_line}");

        // The same hint in --json, next to the stable code.
        let (code, body) = fx.fail_json(args);
        assert_eq!(code, Some(1));
        assert_eq!(body["code"], "not_found", "{args:?}: {body}");
        assert!(
            body["hint"].as_str().is_some_and(|h| h.contains(hint)),
            "{args:?}: {body}"
        );
    }
}

/// The CLI names CLI commands, not the MCP tool, when an agent is unknown.
#[test]
fn an_unknown_agent_hint_names_the_cli_command() {
    let fx = Fx::new();
    let (_, err) = fx.fail(&[
        "send",
        "--from",
        "lead",
        "--to",
        "ghost",
        "--subject",
        "s",
        "--body",
        "b",
    ]);
    let hint = err.lines().find(|l| l.starts_with("hint: ")).unwrap_or("");
    assert!(hint.contains("another agent register"), "{err}");
    assert!(!hint.contains("register_agent"), "{err}");
}

/// A conflict exits 3 everywhere, not only for holds: here, acting on a closed request.
#[test]
fn every_conflict_exits_3() {
    let fx = Fx::new();
    let sent = fx.ok(&[
        "--json",
        "send",
        "--from",
        "lead",
        "--to",
        "worker",
        "--subject",
        "Port it",
        "--body",
        "b",
        "--request",
    ]);
    let rid = serde_json::from_str::<Value>(&sent).unwrap()["request_id"]
        .as_str()
        .unwrap()
        .to_string();
    fx.ok(&["request", "claim", "--agent", "worker", &rid]);
    fx.ok(&["request", "complete", "--agent", "worker", &rid]);

    let (code, err) = fx.fail(&["request", "claim", "--agent", "lead", &rid]);
    assert_eq!(code, Some(3), "{err}");
    let (code, body) = fx.fail_json(&["request", "claim", "--agent", "lead", &rid]);
    assert_eq!(code, Some(3));
    assert_eq!(body["code"], "conflict");

    // Completing a request another agent has claimed is a conflict too (README's example).
    let sent = fx.ok(&[
        "--json",
        "send",
        "--from",
        "lead",
        "--to",
        "worker",
        "--subject",
        "Second",
        "--body",
        "b",
        "--request",
    ]);
    let second = serde_json::from_str::<Value>(&sent).unwrap()["request_id"]
        .as_str()
        .unwrap()
        .to_string();
    fx.ok(&["request", "claim", "--agent", "worker", &second]);
    let (code, body) = fx.fail_json(&["request", "complete", "--agent", "lead", &second]);
    assert_eq!(code, Some(3), "{body}");
    assert_eq!(body["code"], "conflict");
    // Releasing it is `forbidden`, not a conflict, and keeps status 1 (review R1 of #156).
    let (code, body) = fx.fail_json(&["request", "release", "--agent", "lead", &second]);
    assert_eq!(code, Some(1), "{body}");
    assert_eq!(body["code"], "forbidden");

    // The hold conflict keeps its exit status 3.
    fx.ok(&["hold", "acquire", "--agent", "lead", "src/a.rs"]);
    let out = fx.run(&["hold", "check", "--agent", "worker", "src/a.rs"]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out.stdout));
}

/// Errors that are not conflicts keep status 1, and bad arguments keep clap's 2.
#[test]
fn other_errors_exit_1_and_bad_arguments_exit_2() {
    let fx = Fx::new();
    let (code, _) = fx.fail(&["activity", "--since", "yesterday"]);
    assert_eq!(code, Some(1));
    let (code, _) = fx.fail(&["inbox", "--agent", "nobody"]);
    assert_eq!(code, Some(1));
    let (code, err) = fx.fail(&["inbox", "--no-such-flag"]);
    assert_eq!(code, Some(2), "{err}");
}

/// A message that already says what to do gets no extra hint line.
#[test]
fn self_explaining_errors_get_no_hint() {
    let fx = Fx::new();
    let (_, err) = fx.fail(&["activity", "--since", "yesterday"]);
    assert!(err.contains("use a duration like"), "{err}");
    assert!(!err.contains("hint: "), "{err}");
}
