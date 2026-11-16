//! The binary must not depend on a large main-thread stack. Windows gives a debug build's
//! main thread 1 MB, and clap's derive uses nearly all of it, so the CLI runs on its own
//! thread (`cli::with_cli_stack`). A 256 KB limit here (via `ulimit -s`) leaves no doubt.
#![cfg(unix)]

use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_another");

#[test]
fn the_cli_runs_with_a_small_main_thread_stack() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        "init",
        "--version",
        "status --help",
        "nosuchcommand",
        "plugin-x",
    ] {
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("ulimit -s 256 && exec \"$0\" {args}"))
            .arg(BIN)
            .env("ANOTHER_HOME", dir.path())
            .env_remove("ANOTHER_AGENT")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!err.contains("overflowed its stack"), "{args}: {err}");
        assert!(
            out.status.code().is_some(),
            "{args}: killed by a signal: {err}"
        );
    }
}

/// The thread wrapper keeps exit statuses: success, clap's usage error and a conflict.
#[test]
fn exit_statuses_pass_through_the_thread() {
    let dir = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        Command::new(BIN)
            .args(args)
            .env("ANOTHER_HOME", dir.path())
            .env_remove("ANOTHER_AGENT")
            .stdin(Stdio::null())
            .output()
            .unwrap()
            .status
            .code()
    };
    assert_eq!(run(&["init"]), Some(0));
    assert_eq!(run(&["--no-such-flag"]), Some(2));
    assert_eq!(run(&["agent", "show", "nobody"]), Some(1));
}
