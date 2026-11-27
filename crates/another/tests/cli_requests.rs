//! `another request claim|release|cancel|complete` as an operator or script sees them: who
//! may do what, and the exit status each refusal gives (3 for a conflict with the request's
//! current state, 1 for anything else). The core rules are tested in flows.rs; nothing ran
//! the CLI's release and cancel paths.

use std::path::PathBuf;
use std::process::{Command, Stdio};

struct Fx {
    _dir: tempfile::TempDir,
    home: PathBuf,
    shop: PathBuf,
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
        fx.code(&["init"]);
        for name in ["lead", "worker", "other"] {
            assert_eq!(
                fx.code(&[
                    "agent",
                    "register",
                    name,
                    "--program",
                    "codex",
                    "--project",
                    "."
                ]),
                (0, String::new())
            );
        }
        fx
    }

    /// Exit status and stderr.
    fn code(&self, args: &[&str]) -> (i32, String) {
        let out = {
            let mut legacy_fixture_command = Command::new(env!("CARGO_BIN_EXE_another"));
            legacy_fixture_command.arg("--unguarded-repair");
            legacy_fixture_command
        }
        .args(args)
        .current_dir(&self.shop)
        .env("ANOTHER_HOME", &self.home)
        .env_remove("ANOTHER_AGENT")
        .env_remove("ANOTHER_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .unwrap();
        (
            out.status.code().unwrap(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn request(&self) -> String {
        let out = {
            let mut legacy_fixture_command = Command::new(env!("CARGO_BIN_EXE_another"));
            legacy_fixture_command.arg("--unguarded-repair");
            legacy_fixture_command
        }
        .args([
            "--json",
            "send",
            "--from",
            "lead@shop",
            "--to",
            "worker@shop",
            "--request",
            "--subject",
            "Do it",
            "--body",
            "b",
        ])
        .current_dir(&self.shop)
        .env("ANOTHER_HOME", &self.home)
        .env_remove("ANOTHER_AGENT")
        .output()
        .unwrap();
        assert!(out.status.success());
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        v["request_id"].as_str().unwrap().to_string()
    }

    fn act(&self, action: &str, who: &str, id: &str) -> (i32, String) {
        self.code(&["request", action, "--agent", who, id])
    }
}

#[test]
fn claim_release_and_cancel_follow_ownership() {
    let fx = Fx::new();
    let id = fx.request();

    // Nobody owns it yet: releasing it is refused (exit 1).
    let (code, err) = fx.act("release", "worker@shop", &id);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("owned by nobody"), "{err}");

    assert_eq!(fx.act("claim", "worker@shop", &id).0, 0);
    // Someone else claiming it conflicts with its state (exit 3) and names the owner.
    let (code, err) = fx.act("claim", "other@shop", &id);
    assert_eq!(code, 3, "{err}");
    assert!(err.contains("claimed by worker@shop"), "{err}");

    // Only its creator may cancel it, whoever holds it.
    for who in ["worker@shop", "other@shop"] {
        let (code, err) = fx.act("cancel", who, &id);
        assert_eq!(code, 1, "{who}: {err}");
        assert!(err.contains("only the creator"), "{who}: {err}");
    }

    // The owner gives it back, and the creator cancels it.
    assert_eq!(fx.act("release", "worker@shop", &id).0, 0);
    assert_eq!(fx.act("cancel", "lead@shop", &id).0, 0);

    // A cancelled request can be neither claimed nor completed (exit 3).
    for action in ["claim", "complete"] {
        let (code, err) = fx.act(action, "worker@shop", &id);
        assert_eq!(code, 3, "{action}: {err}");
        assert!(err.contains("already cancelled"), "{action}: {err}");
    }
}

#[test]
fn an_unknown_request_is_exit_1_with_a_hint() {
    let fx = Fx::new();
    for action in ["claim", "release", "cancel", "complete"] {
        let (code, err) = fx.act(action, "lead@shop", "nosuch");
        assert_eq!(code, 1, "{action}: {err}");
        assert!(
            err.contains("unknown request \"nosuch\""),
            "{action}: {err}"
        );
        assert!(err.contains("another requests"), "{action}: {err}");
    }
}
