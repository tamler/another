//! Human (non-JSON) output never passes a terminal control sequence through. Held paths,
//! agent and project names, subjects and reasons come from other agents and other
//! machines; a file name can hold ESC or BEL.

use std::path::Path;
use std::process::{Command, Output, Stdio};

fn another(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_another"))
        .args(args)
        .current_dir(cwd)
        .env("ANOTHER_HOME", home)
        .env_remove("ANOTHER_AGENT")
        .env_remove("ANOTHER_LOG")
        .stdin(Stdio::null())
        .output()
        .expect("run another")
}

/// ESC, BEL, CSI and the bidi overrides D75 refuses in subjects.
fn has_control(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    text.chars()
        .any(|c| c != '\n' && c != '\t' && another_core::is_unsafe_text_char(c))
}

#[test]
fn a_held_file_name_cannot_retitle_or_clear_the_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let shop = dir.path().join("shop");
    std::fs::create_dir_all(&shop).unwrap();
    let run = |args: &[&str]| another(&home, &shop, args);
    assert!(run(&["init"]).status.success());
    assert!(run(&[
        "agent",
        "register",
        "a",
        "--program",
        "claude",
        "--project",
        "."
    ])
    .status
    .success());
    assert!(run(&[
        "agent",
        "register",
        "b",
        "--program",
        "codex",
        "--project",
        "."
    ])
    .status
    .success());
    // OSC 0 (set the terminal title), ended by BEL; and a right-to-left override.
    let hostile = "src/x\u{1b}]0;pwned\u{7}\u{202e}gpj.rs";
    let out = run(&["hold", "acquire", "-a", "a", hostile]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    for args in [
        &["holds"][..],
        &["board"][..],
        &["activity", "--since", "1h"][..],
    ] {
        let out = run(args);
        assert!(out.status.success(), "{args:?}");
        assert!(
            !has_control(&out.stdout),
            "{args:?} printed a control character: {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("src/x?]0;pwned??gpj.rs"),
            "{args:?}: {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    // A conflict report goes to stdout too, and an error message to stderr.
    let out = run(&["hold", "check", "-a", "b", hostile]);
    assert!(
        !has_control(&out.stdout),
        "{:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    let out = run(&["hold", "release", "-a", "b", "nosuch"]);
    assert!(!has_control(&out.stderr));

    // --json keeps the exact text, escaped by JSON.
    let out = run(&["--json", "holds"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v.to_string().contains("\\u001b]0;pwned"), "{v}");
}
