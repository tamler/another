//! `another send --template`: a templated request through the real CLI.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::Value;

fn another(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_another"))
        .args(args)
        .env("ANOTHER_HOME", home)
        .env_remove("ANOTHER_AGENT")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn setup() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["init"],
        vec!["agent", "register", "--program", "claude", "lead"],
        vec!["agent", "register", "--program", "codex", "bob"],
    ] {
        let out = another(dir.path(), &args);
        assert!(out.status.success(), "{args:?}");
    }
    dir
}

#[test]
fn key_value_flags_trim_keys_and_preserve_values() {
    let dir = setup();
    let sent = another(
        dir.path(),
        &[
            "--json",
            "send",
            "--from",
            "lead",
            "--to",
            "bob",
            "--template",
            "fix",
            "--field",
            " issue =parser=a=b",
        ],
    );
    assert!(sent.status.success(), "{sent:?}");
    let sent: Value = serde_json::from_slice(&sent.stdout).unwrap();
    assert_eq!(sent["subject"], "Fix: parser=a=b");

    let registered = another(
        dir.path(),
        &[
            "--json",
            "resource",
            "register",
            "model_server",
            "parser-test",
            "--agent",
            "lead",
            "--meta",
            " backend = metal=a=b ",
            "--meta",
            "empty=",
        ],
    );
    assert!(registered.status.success(), "{registered:?}");
    let registered: Value = serde_json::from_slice(&registered.stdout).unwrap();
    assert_eq!(registered["resource"]["metadata"]["backend"], " metal=a=b ");
    assert_eq!(registered["resource"]["metadata"]["empty"], "");
}

#[test]
fn key_value_flags_refuse_malformed_and_duplicate_keys_with_usage_exit() {
    let dir = setup();
    for (base, flag, key) in [
        (
            vec!["send", "--from", "lead", "--to", "bob", "--template", "fix"],
            "--field",
            "issue",
        ),
        (
            vec![
                "resource",
                "register",
                "model_server",
                "parser-test",
                "--agent",
                "lead",
            ],
            "--meta",
            "backend",
        ),
    ] {
        for (pairs, expected) in [
            (vec!["noequals".to_string()], "key=value"),
            (vec!["=value".to_string()], "empty"),
            (vec![" \t =value".to_string()], "empty"),
            (vec![format!("{key}=a"), format!("{key}=b")], "twice"),
            (vec![format!(" {key} =a"), format!("{key}=b")], "twice"),
        ] {
            for json in [false, true] {
                let mut args = Vec::new();
                if json {
                    args.push("--json");
                }
                args.extend(base.iter().copied());
                for pair in &pairs {
                    args.extend([flag, pair.as_str()]);
                }
                let out = another(dir.path(), &args);
                assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
                let message = if json {
                    let error: Value = serde_json::from_slice(&out.stdout).unwrap();
                    assert_eq!(error["error"]["code"], "usage", "{error}");
                    error["error"]["message"].as_str().unwrap().to_string()
                } else {
                    String::from_utf8_lossy(&out.stderr).into_owned()
                };
                assert!(message.contains(flag), "{message}");
                assert!(message.contains(expected), "{message}");
            }
        }
    }
    let resources = another(dir.path(), &["--json", "resources"]);
    assert!(resources.status.success(), "{resources:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&resources.stdout).unwrap(),
        serde_json::json!([])
    );
}

#[test]
fn a_template_opens_a_request_with_its_subject_and_sections() {
    let dir = setup();
    let out = another(
        dir.path(),
        &[
            "--json",
            "send",
            "--from",
            "lead",
            "--to",
            "bob",
            "--template",
            "review",
            "--field",
            "target=PR #170",
            "--field",
            "focus=error paths",
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let msg: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(msg["subject"], "Review: PR #170");
    let body = msg["body"].as_str().unwrap();
    assert!(body.contains("## Focus\n\n    error paths"), "{body}");
    assert!(body.contains("## Done when"), "{body}");

    let reqs = another(dir.path(), &["--json", "requests"]);
    let reqs: Value = serde_json::from_slice(&reqs.stdout).unwrap();
    assert_eq!(reqs[0]["title"], "Review: PR #170", "{reqs}");
    assert_eq!(reqs[0]["status"], "open");
}

#[test]
fn template_mistakes_are_refused() {
    let dir = setup();
    let base = ["send", "--from", "lead", "--to", "bob", "--template"];
    for (extra, expect) in [
        (vec!["fix"], "--field issue="),
        (
            vec!["fix", "--field", "issue=x", "--field", "nope=1"],
            "no field",
        ),
        (vec!["fix", "--field", "issue=a\nb"], "one line"),
        (
            vec!["fix", "--field", "issue=x", "--field", "issue=y"],
            "twice",
        ),
        (
            vec!["fix", "--field", "issue=x", "--field", "noequals"],
            "key=value",
        ),
        (vec!["nope"], "unknown template"),
    ] {
        let mut args: Vec<&str> = base.to_vec();
        args.extend(extra.iter().copied());
        let out = another(dir.path(), &args);
        // A usage error, like clap's (spec test 2).
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(expect), "{args:?}: {err}");
    }
    // Under --json the same mistake is a JSON usage error.
    let out = another(
        dir.path(),
        &[
            "--json",
            "send",
            "--from",
            "lead",
            "--to",
            "bob",
            "--template",
            "fix",
        ],
    );
    assert_eq!(out.status.code(), Some(2));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["error"]["code"], "usage", "{v}");
    // A template with no sender anywhere is a usage error too.
    let out = another(
        dir.path(),
        &[
            "send",
            "--to",
            "bob",
            "--template",
            "fix",
            "--field",
            "issue=x",
        ],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no sender"));
    // A message without a template is unchanged: clap still requires a sender.
    let out = another(dir.path(), &["send", "--to", "bob", "-s", "hi"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--from"));
    // A template writes the subject and the body itself.
    let out = another(
        dir.path(),
        &[
            "send",
            "--from",
            "lead",
            "--to",
            "bob",
            "--template",
            "fix",
            "--field",
            "issue=x",
            "-s",
            "own",
        ],
    );
    assert_eq!(out.status.code(), Some(2));
    // Nothing was sent by the refused commands.
    let reqs = another(dir.path(), &["--json", "requests"]);
    let reqs: Value = serde_json::from_slice(&reqs.stdout).unwrap();
    assert_eq!(reqs.as_array().unwrap().len(), 0, "{reqs}");

    // `list` prints the templates and their fields, and sends nothing.
    let out = another(
        dir.path(),
        &[
            "send",
            "--from",
            "lead",
            "--to",
            "bob",
            "--template",
            "list",
        ],
    );
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("review") && text.contains("question"),
        "{text}"
    );
}
