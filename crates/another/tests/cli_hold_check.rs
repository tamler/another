//! `another hold check` as a script sees it: exit 0 and "no conflicts" when free, exit 3 with
//! the holder when not, the shared/exclusive rules, and the README's promise that checking
//! grants nothing. Only the exit 3 of one overlap was tested before (cli_errors.rs).

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
        fx.ok(&["init"]);
        for name in ["lead", "worker"] {
            fx.ok(&[
                "agent",
                "register",
                name,
                "--program",
                "codex",
                "--project",
                ".",
            ]);
        }
        fx
    }

    /// Exit status and stdout.
    fn run(&self, args: &[&str]) -> (i32, String) {
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
        .output()
        .unwrap();
        (
            out.status.code().unwrap(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }

    fn ok(&self, args: &[&str]) -> String {
        let (code, out) = self.run(args);
        assert_eq!(code, 0, "{args:?}: {out}");
        out
    }

    fn holds(&self) -> usize {
        let v: serde_json::Value = serde_json::from_str(&self.ok(&["--json", "holds"])).unwrap();
        v.as_array().unwrap().len()
    }
}

#[test]
fn a_free_path_checks_clean_and_checking_grants_nothing() {
    let fx = Fx::new();
    let events = fx.ok(&["--json", "events", "--limit", "1000"]);
    assert_eq!(
        fx.ok(&["hold", "check", "-a", "worker", "src/a.rs"]).trim(),
        "no conflicts"
    );
    assert_eq!(
        fx.ok(&["--json", "hold", "check", "-a", "worker", "src/a.rs"])
            .trim(),
        "[]"
    );
    assert_eq!(fx.holds(), 0, "hold check took a hold");
    assert_eq!(
        fx.ok(&["--json", "events", "--limit", "1000"]),
        events,
        "hold check wrote an event"
    );
    // Nothing was taken, so the lead can still hold the path exclusively.
    fx.ok(&["hold", "acquire", "-a", "lead", "src/a.rs"]);
    assert_eq!(fx.holds(), 1);
}

#[test]
fn an_exclusive_hold_conflicts_for_others_only() {
    let fx = Fx::new();
    fx.ok(&["hold", "acquire", "-a", "lead", "src/a.rs"]);

    let (code, out) = fx.run(&["hold", "check", "-a", "worker", "src/a.rs"]);
    assert_eq!(code, 3, "{out}");
    // The checked path is echoed in the platform's form (src\a.rs on Windows); the hold's
    // own pattern is always stored with '/'.
    assert!(out.starts_with("conflict: src"), "{out}");
    assert!(
        out.contains("is held by lead@shop (shop:src/a.rs, exclusive"),
        "{out}"
    );
    // A glob that covers the held file conflicts too, and JSON names the hold.
    let (code, out) = fx.run(&["--json", "hold", "check", "-a", "worker", "src/*.rs"]);
    assert_eq!(code, 3, "{out}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v[0]["path"], "src/*.rs", "{v}");
    assert_eq!(v[0]["held"]["agent"], "lead@shop", "{v}");
    assert_eq!(v[0]["held"]["pattern"], "src/a.rs", "{v}");
    assert_eq!(v[0]["held"]["exclusive"], true, "{v}");
    // A shared check against an exclusive hold still conflicts.
    let (code, _) = fx.run(&["hold", "check", "-a", "worker", "--shared", "src/a.rs"]);
    assert_eq!(code, 3);

    // The holder's own hold and an unrelated path are clear.
    fx.ok(&["hold", "check", "-a", "lead", "src/a.rs"]);
    fx.ok(&["hold", "check", "-a", "worker", "src/b.rs"]);
    assert_eq!(fx.holds(), 1);
}

#[test]
fn a_shared_hold_admits_shared_checks_but_not_exclusive_ones() {
    let fx = Fx::new();
    fx.ok(&["hold", "acquire", "-a", "lead", "--shared", "docs/x.md"]);
    assert_eq!(
        fx.ok(&["hold", "check", "-a", "worker", "--shared", "docs/x.md"])
            .trim(),
        "no conflicts"
    );
    let (code, out) = fx.run(&["hold", "check", "-a", "worker", "docs/x.md"]);
    assert_eq!(code, 3, "{out}");
    assert!(out.contains("shared until"), "{out}");
}
