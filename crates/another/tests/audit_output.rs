//! `another audit` says so when there is nothing to show, like `peers`, `token list` and
//! `wake list`, instead of printing nothing; JSON stays an empty list.

use std::process::{Command, Stdio};

#[test]
fn an_empty_audit_log_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_another"))
            .args(args)
            .current_dir(dir.path())
            .env("ANOTHER_HOME", dir.path().join("home"))
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_LOG")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    run(&["init"]);
    assert_eq!(
        run(&["audit"]).trim(),
        "no audit entries (remote requests and OAuth events are recorded here)"
    );
    assert_eq!(run(&["--json", "audit"]).trim(), "[]");
}

/// `--limit 0` would show no rows of a log that has some, and the empty-log message would
/// then be false; the limit is refused instead (FINDINGS-207).
#[test]
fn a_zero_limit_is_refused_rather_than_shown_as_an_empty_log() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    another_core::Store::open(&home)
        .unwrap()
        .record_audit(another_core::AuditRecord {
            token_id: None,
            agent: Some("alice".into()),
            operation: "send_message".into(),
            outcome: "ok".into(),
            detail: None,
            remote: None,
        })
        .unwrap();
    for json in [false, true] {
        let mut args = vec!["audit", "--limit", "0"];
        if json {
            args.insert(0, "--json");
        }
        let out = Command::new(env!("CARGO_BIN_EXE_another"))
            .args(&args)
            .current_dir(dir.path())
            .env("ANOTHER_HOME", &home)
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_LOG")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(!out.status.success(), "{args:?} succeeded: {stdout:?}");
        assert!(!stdout.contains("no audit entries"), "{args:?}: {stdout:?}");
        assert_eq!(stdout.trim(), "", "{args:?}");
    }
}
