//! CLI consistency: every argument has help text, every read command speaks `--json`, a
//! bad command line under `--json` is a JSON error, and completion scripts generate.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use clap::CommandFactory;
use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_another");

fn another(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env("ANOTHER_HOME", home)
        .env("HOME", home.join("user-home"))
        .env_remove("ANOTHER_AGENT")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// Every argument and option of every command, at any depth, carries help text, so
/// `--help` never shows a bare `<NAME>`.
#[test]
fn every_argument_has_help_text() {
    fn walk(cmd: &clap::Command, path: &str, missing: &mut Vec<String>) {
        for arg in cmd.get_arguments() {
            let id = arg.get_id().as_str();
            if matches!(id, "help" | "version") {
                continue;
            }
            if arg.get_help().is_none() && arg.get_long_help().is_none() {
                missing.push(format!("{path} {id}"));
            }
        }
        for sub in cmd.get_subcommands() {
            walk(sub, &format!("{path} {}", sub.get_name()), missing);
        }
    }
    let mut missing = Vec::new();
    walk(&another::cli::Cli::command(), "another", &mut missing);
    assert!(
        missing.is_empty(),
        "arguments without help:\n{}",
        missing.join("\n")
    );
}

#[test]
fn every_read_command_prints_json() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let proj = dir.path().join("shop");
    std::fs::create_dir_all(proj.join(".git")).unwrap();
    std::fs::create_dir_all(home.join("user-home")).unwrap();
    let p = proj.to_str().unwrap();
    for args in [
        vec!["init"],
        vec![
            "agent",
            "register",
            "--program",
            "claude",
            "--project",
            p,
            "alice",
        ],
        vec![
            "agent",
            "register",
            "--program",
            "codex",
            "--project",
            p,
            "bob",
        ],
        vec![
            "send",
            "-a",
            "alice@shop",
            "-t",
            "bob@shop",
            "-s",
            "hi",
            "--request",
        ],
    ] {
        let out = another(&home, &proj, &args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let threads = another(&home, &proj, &["--json", "threads", "--agent", "bob@shop"]);
    let threads: Value = serde_json::from_slice(&threads.stdout).unwrap();
    let thread = threads[0]["thread_id"].as_str().unwrap().to_string();

    let reads: Vec<Vec<&str>> = vec![
        vec!["status"],
        vec!["project", "list"],
        vec!["agent", "list"],
        vec!["agent", "show", "alice@shop"],
        vec!["inbox", "--agent", "bob@shop"],
        vec!["threads", "--agent", "bob@shop"],
        vec!["thread", &thread],
        vec!["requests"],
        vec!["holds"],
        vec!["resources"],
        vec!["orphans", "--no-scan"],
        vec!["token", "list"],
        vec!["oauth", "list"],
        vec!["audit"],
        vec!["node", "show"],
        vec!["peers"],
        vec!["activity"],
        vec!["activity", "--summary"],
        vec!["stats"],
        vec!["events"],
        vec!["board"],
        vec!["hook", "status"],
        vec!["wake", "list"],
    ];
    for args in reads {
        let mut all = vec!["--json"];
        all.extend_from_slice(&args);
        let out = another(&home, &proj, &all);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "{args:?} --json is not JSON ({e}): {}",
                String::from_utf8_lossy(&out.stdout)
            )
        });
    }
}

#[test]
fn a_bad_command_line_under_json_is_a_json_error() {
    let dir = tempfile::tempdir().unwrap();
    let out = another(dir.path(), dir.path(), &["--json", "inbox"]);
    assert_eq!(out.status.code(), Some(2));
    let v: Value = serde_json::from_slice(&out.stdout).expect("a JSON error on stdout");
    assert_eq!(v["error"]["code"], "usage");
    assert!(
        v["error"]["message"].as_str().unwrap().contains("--agent"),
        "{v}"
    );

    // Without --json it is clap's usual text on stderr, still exit 2.
    let out = another(dir.path(), dir.path(), &["inbox"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--agent"));

    // --help is still help, even with --json.
    let out = another(dir.path(), dir.path(), &["--json", "inbox", "--help"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Usage"));
}

#[test]
fn completion_scripts_generate_for_every_shell() {
    let dir = tempfile::tempdir().unwrap();
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let out = another(dir.path(), dir.path(), &["completions", shell]);
        assert!(
            out.status.success(),
            "{shell}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let script = String::from_utf8_lossy(&out.stdout);
        assert!(script.contains("another"), "{shell}");
        assert!(script.contains("inbox"), "{shell}: commands are completed");
    }
    // No database is needed, so none is created.
    assert!(!dir.path().join("another.db").exists());
}

/// Review R1-3: after `--` every argument is a value, so a literal `--json` there does not
/// switch the usage error to JSON.
#[test]
fn a_literal_json_after_double_dash_is_not_the_flag() {
    let dir = tempfile::tempdir().unwrap();
    let out = another(dir.path(), dir.path(), &["inbox", "--", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        out.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(!String::from_utf8_lossy(&out.stderr).is_empty());
}

/// Review R1-1: the binary must not depend on a large main-thread stack. Windows gives a
/// debug build's main thread 1 MB, and clap's derive used nearly all of it; the CLI now runs
/// on its own thread. A 256 KB limit here (via `ulimit -s`) leaves no doubt.
#[cfg(unix)]
#[test]
fn the_cli_runs_with_a_small_main_thread_stack() {
    let dir = tempfile::tempdir().unwrap();
    for args in ["init", "--version", "status --help", "nosuchcommand"] {
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
