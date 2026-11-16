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
