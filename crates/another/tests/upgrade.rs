//! Upgrade safety through the binary (scope M3). The test binary is a development build, as
//! the one run against the live home on 17 Nov 2026 was: against an existing home at an
//! older schema it must refuse and write nothing, until `--dev-migrate` says otherwise.

use another_core::test_support::{older_home as make_older_home, schema_version as version};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

#[path = "common/cli.rs"]
mod cli;

fn another(home: &Path, args: &[&str]) -> Output {
    cli::another_cmd(home)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("run another")
}

/// A home one schema behind this build, with no record of the build that made it: what the
/// installed binary's home looks like to a newer development build.
fn older_home(dir: &Path) -> (PathBuf, i64) {
    let home = dir.join("home");
    let older = make_older_home(&home, 1);
    (home, older + 1)
}

#[test]
fn a_development_binary_refuses_an_older_home_until_told_to_migrate_it() {
    let dir = tempfile::tempdir().unwrap();
    let (home, current) = older_home(dir.path());
    let db = home.join("another.db");
    let before = std::fs::read(&db).unwrap();

    // Any normal command: text and JSON, exit 1, nothing written.
    let out = another(&home, &["status"]);
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(
        text.contains("this development build of another")
            && text.contains("will not migrate the database")
            && text.contains(&format!("v{} to v{current}", current - 1))
            && text.contains("--dev-migrate"),
        "{text}"
    );
    let out = another(&home, &["--json", "status"]);
    assert_eq!(out.status.code(), Some(1));
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(body["error"]["code"], "invalid", "{body}");
    assert_eq!(std::fs::read(&db).unwrap(), before, "a refused run wrote");
    assert_eq!(version(&db), current - 1);
    assert!(!home.join("backups").exists());
    assert!(!home.join("build.json").exists());

    // The explicit flag migrates it, after a backup of the old schema.
    let out = another(&home, &["--dev-migrate", "status"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(version(&db), current);
    let backups: Vec<PathBuf> = std::fs::read_dir(home.join("backups"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(backups.len(), 1, "{backups:?}");
    assert_eq!(version(&backups[0]), current - 1);
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.join("build.json")).unwrap()).unwrap();
    assert_eq!(record["migrated"]["build"], "development", "{record}");
    assert_eq!(record["migrated"]["schema"], current, "{record}");

    // The flag was for that run only; the home is current now, so nothing needs it.
    assert!(another(&home, &["status"]).status.success());
}

/// While a home is refused, hooks still fail open and stay quiet, as hooks must. The reason
/// a person has to act on goes to `logs/hooks.log`, in the store's own words, once.
#[test]
fn a_refused_home_keeps_hooks_quiet_and_logs_the_true_reason_once() {
    let dir = tempfile::tempdir().unwrap();
    let (home, _) = older_home(dir.path());
    let db = home.join("another.db");
    let before = std::fs::read(&db).unwrap();
    let user = dir.path().join("user");
    std::fs::create_dir_all(&user).unwrap();

    for event in ["session-start", "prompt", "pre-edit", "post-tool", "prompt"] {
        let out = cli::another_cmd(&home)
            .args(["hook", "run", event, "--client", "claude"])
            .env("HOME", &user)
            .env("USERPROFILE", &user)
            .stdin(Stdio::null())
            .output()
            .expect("run another");
        assert_eq!(out.status.code(), Some(0), "{event}");
        assert!(out.stdout.is_empty(), "{event}: hooks stay quiet");
    }

    let log = std::fs::read_to_string(home.join("logs/hooks.log")).unwrap();
    let reasons: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("will not migrate the database"))
        .collect();
    assert_eq!(reasons.len(), 1, "{log}");
    assert!(reasons[0].contains("--dev-migrate"), "{log}");
    assert!(!log.contains("a normal open migrates it"), "{log}");
    assert_eq!(std::fs::read(&db).unwrap(), before, "a hook wrote");
}

#[test]
fn a_home_this_build_created_needs_no_flag() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    assert!(another(&home, &["init"]).status.success());
    assert_eq!(
        version(&home.join("another.db")),
        another_core::test_support::current_schema_version(),
        "the fixture schema must match the real binary's init"
    );
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.join("build.json")).unwrap()).unwrap();
    assert_eq!(record["created"]["build"], "development", "{record}");
    assert!(another(&home, &["status"]).status.success());
    assert!(!home.join("backups").exists(), "no migration, no backup");
}
