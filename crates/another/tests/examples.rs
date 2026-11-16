//! The scripts in `examples/` run end to end against this build, so they cannot drift
//! from the CLI. Each script checks its own outcome and exits non-zero when it is wrong.
#![cfg(unix)]

#[path = "common/hosted_ci.rs"]
mod hosted_ci;

use std::path::PathBuf;
use std::process::{Command, Stdio};

fn run_example(name: &str) -> String {
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples")
        .join(name);
    let scratch = tempfile::tempdir().unwrap();
    let out = Command::new("sh")
        .arg(&script)
        .env("ANOTHER_BIN", env!("CARGO_BIN_EXE_another"))
        .env("TMPDIR", scratch.path())
        .env("HOME", scratch.path())
        .env("ANOTHER_LOG", "off")
        .env_remove("ANOTHER_HOME")
        .env_remove("ANOTHER_AGENT")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{name} failed ({}):\n{stdout}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    // The script cleans up after itself.
    let left: Vec<_> = std::fs::read_dir(scratch.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .filter(|n| n.to_string_lossy().starts_with("another-"))
        .collect();
    assert!(left.is_empty(), "{name} left {left:?} behind");
    stdout
}

#[test]
fn two_agent_handoff() {
    let out = run_example("two-agent-handoff.sh");
    assert!(out.contains("handoff complete"), "{out}");
}

/// Starts a federation hub in the background and stops it with SIGTERM by its process id.
#[test]
fn federation_hub_two_spokes() {
    hosted_ci::require_hosted_runner();
    let out = run_example("federation-hub-two-spokes.sh");
    assert!(out.contains("federation round trip complete"), "{out}");
}
