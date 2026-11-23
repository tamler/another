//! Upgrade safety through the binary (scope M3). The test binary is a development build, as
//! the one run against the live home on 17 Nov 2026 was: against an existing home at an
//! older schema it must refuse and write nothing, until `--dev-migrate` says otherwise.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn another(home: &Path, args: &[&str]) -> Output {
    {
        let mut legacy_fixture_command = Command::new(env!("CARGO_BIN_EXE_another"));
        legacy_fixture_command.arg("--unguarded-repair");
        legacy_fixture_command
    }
    .args(args)
    .env("ANOTHER_HOME", home)
    .env_remove("ANOTHER_AGENT")
    .env_remove("ANOTHER_LOG")
    .stdin(Stdio::null())
    .output()
    .expect("run another")
}

fn version(db: &Path) -> i64 {
    rusqlite::Connection::open(db)
        .unwrap()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap()
}

/// A home one schema behind this build, with no record of the build that made it: what the
/// installed binary's home looks like to a newer development build.
fn older_home(dir: &Path) -> (PathBuf, i64) {
    let fresh = dir.join("fresh");
    assert!(another(&fresh, &["init"]).status.success());
    let current = version(&fresh.join("another.db"));
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let c = rusqlite::Connection::open(home.join("another.db")).unwrap();
    c.pragma_update(None, "journal_mode", "WAL").unwrap();
    another_core::apply_schema(&c, current - 1).unwrap();
    (home, current)
}

#[test]
fn a_development_binary_refuses_an_older_home_until_told_to_migrate_it() {
    let dir = tempfile::tempdir().unwrap();
    let (home, current) = older_home(dir.path());
    let db = home.join("another.db");
    let before = std::fs::read(&db).unwrap();

    // Any normal command: text and JSON, exit 1, nothing written.
    let out = another(&home, &["init"]);
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(
        text.contains("this development build of another")
            && text.contains("will not migrate the database")
            && text.contains(&format!("v{} to v{current}", current - 1))
            && text.contains("--dev-migrate"),
        "{text}"
    );
    let out = another(&home, &["--json", "init"]);
    assert_eq!(out.status.code(), Some(1));
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(body["error"]["code"], "invalid", "{body}");
    assert_eq!(std::fs::read(&db).unwrap(), before, "a refused run wrote");
    assert_eq!(version(&db), current - 1);
    assert!(!home.join("backups").exists());
    assert!(!home.join("build.json").exists());

    // The explicit flag migrates it, after a backup of the old schema.
    let out = another(&home, &["--dev-migrate", "init"]);
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

/// Refused schema never admits an edit and never migrates during admission.
#[test]
fn refused_schema_denies_participating_edits_without_migration() {
    let dir = tempfile::tempdir().unwrap();
    let (home, _) = older_home(dir.path());
    let db = home.join("another.db");
    let before = std::fs::read(&db).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["hook", "run", "pre-edit", "--client", "claude"])
        .env("ANOTHER_HOME", &home)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success());
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(body["hookSpecificOutput"]["permissionDecision"], "deny");
    assert_eq!(std::fs::read(&db).unwrap(), before);
    assert!(!home.join("backups").exists());
}

#[test]
fn a_home_this_build_created_needs_no_flag() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    assert!(another(&home, &["init"]).status.success());
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.join("build.json")).unwrap()).unwrap();
    assert_eq!(record["created"]["build"], "development", "{record}");
    assert!(another(&home, &["status"]).status.success());
    assert!(!home.join("backups").exists(), "no migration, no backup");
}
