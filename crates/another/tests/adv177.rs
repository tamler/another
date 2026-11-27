//! Adversarial pass on #177 (request templates). `gap_*` tests fail at the PR's head;
//! `control_*` tests pin what should stay.

use std::path::Path;
use std::process::{Command, Output, Stdio};

fn another(home: &Path, agent: Option<&str>, args: &[&str]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_another"));
    c.args(args)
        .env("ANOTHER_HOME", home)
        .env_remove("ANOTHER_AGENT")
        .env_remove("ANOTHER_LOG")
        .stdin(Stdio::null());
    if let Some(a) = agent {
        c.env("ANOTHER_AGENT", a);
    }
    c.output().unwrap()
}

fn home() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    // The child's status and stderr are printed on failure (Windows CI showed only that
    // `init` failed).
    let ok = |out: Output| {
        assert!(
            out.status.success(),
            "{:?}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )
    };
    ok(another(&home, None, &["init"]));
    for name in ["lead", "worker"] {
        ok(another(
            &home,
            None,
            &["agent", "register", name, "--program", "claude"],
        ));
    }
    (dir, home)
}

/// T1. The PR and its help say `another send --template list` shows the templates. Listing
/// sends nothing and needs no sender, but `--from` is still a required argument, so the
/// documented command exits 2 unless `ANOTHER_AGENT` happens to be set.
#[test]
fn gap_template_list_needs_no_sender() {
    let (_d, home) = home();
    let out = another(&home, None, &["send", "--template", "list"]);
    assert!(
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("review"),
        "exit {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// T2. With `--json`, every other read command prints JSON; `--template list` prints text.
#[test]
fn gap_template_list_honours_json() {
    let (_d, home) = home();
    let out = another(
        &home,
        Some("lead"),
        &["--json", "send", "--template", "list"],
    );
    assert!(out.status.success());
    let parsed: Result<serde_json::Value, _> = serde_json::from_slice(&out.stdout);
    assert!(
        parsed.is_ok(),
        "--json printed: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// T3. "A missing or unknown field is refused, with the field named." A main field with a
/// tab is refused by the generic subject rule, which does not say that the `target` field
/// caused it. (Author's change: the over-long case of this test now expects the subject to
/// be cut, as SPEC.md says and REVIEW-R1 asked; see `a_long_main_field_is_cut_in_the_subject`.)
#[test]
fn gap_a_bad_main_field_is_refused_with_the_field_named() {
    let (_d, home) = home();
    for target in ["a\tb", "a\u{1b}[2Jb"] {
        let out = another(
            &home,
            None,
            &[
                "send",
                "--from",
                "lead",
                "--to",
                "worker",
                "--template",
                "review",
                "--field",
                &format!("target={target}"),
            ],
        );
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success());
        assert!(
            err.contains("target"),
            "refused without naming the field: {err}"
        );
    }
}

/// Control: a valid template opens a request whose title is the rendered subject.
#[test]
fn control_a_template_opens_a_request() {
    let (_d, home) = home();
    let out = another(
        &home,
        None,
        &[
            "--json",
            "send",
            "--from",
            "lead",
            "--to",
            "worker",
            "--template",
            "review",
            "--field",
            "target=PR #170",
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["subject"], "Review: PR #170");
    assert!(v["request_id"].is_string());
}
