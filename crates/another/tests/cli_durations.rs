//! `--expires` durations (`token create`, `pair invite`) set how long a credential lives, so
//! each unit must mean what it says, and a duration that is not a number with one of the
//! units s, m, h, d or w must be refused rather than read as something else.
//! (Overflow is covered by `cli.rs` `absurd_durations_are_errors_not_panics`.)

use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::Value;

fn another(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_another"))
        .args(args)
        .current_dir(home)
        .env("ANOTHER_HOME", home.join("home"))
        .env_remove("ANOTHER_AGENT")
        .env_remove("ANOTHER_LOG")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn ts(v: &Value) -> i64 {
    another_core::parse_ts(v.as_str().unwrap()).unwrap()
}

#[test]
fn each_unit_sets_the_expiry_it_names() {
    let dir = tempfile::tempdir().unwrap();
    assert!(another(
        dir.path(),
        &["agent", "register", "alice", "--program", "claude"]
    )
    .status
    .success());
    for (expires, secs) in [
        ("90", 90),
        ("90s", 90),
        ("45m", 45 * 60),
        ("5h", 5 * 3600),
        ("3d", 3 * 86_400),
        ("2w", 2 * 7 * 86_400),
        (" 2w ", 2 * 7 * 86_400),
    ] {
        let out = another(
            dir.path(),
            &[
                "--json",
                "token",
                "create",
                "--agent",
                "alice",
                "--expires",
                expires,
            ],
        );
        assert!(
            out.status.success(),
            "{expires:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        let lifetime = ts(&v["expires_at"]) - ts(&v["created_at"]);
        assert_eq!(lifetime, secs * 1000, "{expires:?}: {v}");
    }
}

#[test]
fn a_duration_must_be_a_number_with_a_known_unit() {
    let dir = tempfile::tempdir().unwrap();
    assert!(another(
        dir.path(),
        &["agent", "register", "alice", "--program", "claude"]
    )
    .status
    .success());
    for (expires, says) in [
        ("5y", "use s, m, h, d or w"),
        ("5M", "use s, m, h, d or w"),
        ("5 h", "use s, m, h, d or w"),
        ("5hm", "use s, m, h, d or w"),
        ("h", "invalid duration"),
        ("", "invalid duration"),
        ("-5m", "invalid duration"),
        ("1.5h", "use s, m, h, d or w"),
        // Parses, but a token that is born expired is refused, in any unit.
        ("0", "expiry must be positive"),
        ("0w", "expiry must be positive"),
    ] {
        // `--expires=…` so a leading `-` reaches the parser instead of clap.
        let arg = format!("--expires={expires}");
        let out = another(dir.path(), &["token", "create", "--agent", "alice", &arg]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{expires:?}: {stderr}");
        assert!(stderr.contains(says), "{expires:?}: {stderr}");
    }
    // Nothing was created by the refused calls.
    let list = another(dir.path(), &["--json", "token", "list"]);
    let v: Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(v.as_array().map(Vec::len), Some(0), "{v}");
    // The same parser guards a pairing invite.
    assert!(another(dir.path(), &["node", "name", "hub"])
        .status
        .success());
    let out = another(
        dir.path(),
        &[
            "pair",
            "invite",
            "--url",
            "https://hub.example",
            "--expires",
            "2y",
        ],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("use s, m, h, d or w"), "{stderr}");
}
