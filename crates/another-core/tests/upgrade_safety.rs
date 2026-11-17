//! Upgrade safety (scope M3). On 17 Nov 2026 a development build was run against the live
//! home, migrated its database and broke every agent on the installed binary. These tests
//! hold the rules that stop it: a development build refuses a home it did not create, every
//! migration leaves a backup that restores, and a schema this build does not know is refused.

use std::path::{Path, PathBuf};

use another_core::*;
use rusqlite::Connection;

const PEER: &str = "01PEERPEERPEERPEERPEERPEER";

const RELEASE: Build = Build {
    development: false,
    allow_dev_migrate: false,
};
const DEVELOPMENT: Build = Build {
    development: true,
    allow_dev_migrate: false,
};

fn current() -> i64 {
    let dir = tempfile::tempdir().unwrap();
    Store::open(dir.path()).unwrap();
    version(&dir.path().join(DB_FILE))
}

fn version(db: &Path) -> i64 {
    Connection::open(db)
        .unwrap()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap()
}

fn peers(db: &Path) -> i64 {
    Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM nodes WHERE node_id = ?1",
            [PEER],
            |r| r.get(0),
        )
        .unwrap()
}

/// A home as an earlier build left it: one schema behind, with a row to lose, and no record
/// of which build made it (homes from before the record existed have none).
fn older_home(dir: &Path) -> PathBuf {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let c = Connection::open(home.join(DB_FILE)).unwrap();
    // Every home a build made is in WAL mode.
    c.pragma_update(None, "journal_mode", "WAL").unwrap();
    apply_schema(&c, current() - 1).unwrap();
    c.execute(
        "INSERT INTO nodes (node_id, display_name, role, is_local, created_at)
         VALUES (?1, 'other', 'peer', 0, 0)",
        [PEER],
    )
    .unwrap();
    home
}

fn record(home: &Path, build: &str) {
    std::fs::write(
        home.join(ORIGIN_FILE),
        format!(
            r#"{{"created":{{"version":"0.1.0","build":"{build}","schema":{},"at":"2026-11-01T00:00:00.000Z"}}}}"#,
            current() - 1
        ),
    )
    .unwrap();
}

fn origin(home: &Path) -> HomeOrigin {
    serde_json::from_slice(&std::fs::read(home.join(ORIGIN_FILE)).unwrap()).unwrap()
}

fn backups(home: &Path) -> Vec<PathBuf> {
    let Ok(dir) = std::fs::read_dir(home.join(BACKUP_DIR)) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = dir.map(|e| e.unwrap().path()).collect();
    found.sort();
    found
}

/// Every file in the home with a digest of its bytes: a refused open must leave all of it
/// alone. SQLite's `-wal` and `-shm` files are left out: any connection may create them empty.
fn files(home: &Path) -> Vec<(PathBuf, usize, u64)> {
    use std::hash::{Hash, Hasher};
    let mut found: Vec<(PathBuf, usize, u64)> = std::fs::read_dir(home)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            let name = p.to_string_lossy();
            !name.ends_with("-wal") && !name.ends_with("-shm")
        })
        .map(|p| {
            let bytes = std::fs::read(&p).unwrap_or_default();
            let mut digest = std::collections::hash_map::DefaultHasher::new();
            bytes.hash(&mut digest);
            (p, bytes.len(), digest.finish())
        })
        .collect();
    found.sort();
    found
}

/// The 17 Nov failure: a development build opens an existing home at an older schema.
#[test]
fn a_development_build_refuses_to_migrate_a_home_it_did_not_create() {
    assert!(
        Build::current().development,
        "the test profile is a development build"
    );
    for recorded in [None, Some("release")] {
        let dir = tempfile::tempdir().unwrap();
        let home = older_home(dir.path());
        if let Some(build) = recorded {
            record(&home, build);
        }
        let before = files(&home);

        // `Store::open` is what every normal command runs.
        let err = Store::open(&home)
            .err()
            .expect("a development build must refuse this home");
        assert_eq!(err.code(), "invalid", "{err}");
        let text = err.to_string();
        assert!(
            text.contains("development build")
                && text.contains(&format!("v{}", current() - 1))
                && text.contains("--dev-migrate")
                && text.contains("ANOTHER_HOME"),
            "{text}"
        );
        assert_eq!(files(&home), before, "a refused open wrote to the home");
        assert!(backups(&home).is_empty());
        assert_eq!(version(&home.join(DB_FILE)), current() - 1);
        assert!(Store::open_as(&home, DEVELOPMENT).is_err());
    }
}

#[test]
fn a_release_build_migrates_without_a_flag_and_leaves_a_backup_that_restores() {
    let dir = tempfile::tempdir().unwrap();
    let home = older_home(dir.path());
    let db = home.join(DB_FILE);
    let old = current() - 1;

    let store = Store::open_as(&home, RELEASE).unwrap();
    assert_eq!(version(&db), current());
    assert_eq!(peers(&db), 1, "the migration kept the data");
    let migrated = origin(&home).migrated.expect("the migration is recorded");
    assert_eq!(
        (migrated.build.as_str(), migrated.schema),
        ("release", current())
    );
    assert!(
        origin(&home).created.is_none(),
        "this build did not create it"
    );

    let saved = backups(&home);
    assert_eq!(saved.len(), 1, "{saved:?}");
    let name = saved[0].file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with(&format!("another-v{old}-")) && name.ends_with(".db"),
        "{name}"
    );
    assert_eq!(version(&saved[0]), old);
    assert_eq!(peers(&saved[0]), 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&saved[0]), 0o600);
        assert_eq!(mode(&home.join(BACKUP_DIR)), 0o700);
    }

    // Work after the upgrade, then restore as docs/DOCTOR.md says: stop every process, copy
    // the backup over the database and remove the WAL files.
    store
        .register_agent(RegisterAgent {
            name: "alice".into(),
            program: Program::Claude,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    drop(store);
    std::fs::copy(&saved[0], &db).unwrap();
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(home.join(format!("{DB_FILE}{suffix}")));
    }
    assert_eq!(version(&db), old, "the restored database is the old schema");
    assert_eq!(peers(&db), 1);
    let check: String = Connection::open(&db)
        .unwrap()
        .query_row("PRAGMA quick_check(1)", [], |r| r.get(0))
        .unwrap();
    assert_eq!(check, "ok");

    // The restored home upgrades again, with a second backup, and is back before `alice`.
    let store = Store::open_as(&home, RELEASE).unwrap();
    assert_eq!(version(&db), current());
    assert!(
        store.agent("alice").is_err(),
        "the restore undid later work"
    );
    assert_eq!(backups(&home).len(), 2);
}

#[test]
fn the_explicit_flag_lets_a_development_build_migrate_after_a_backup() {
    let dir = tempfile::tempdir().unwrap();
    let home = older_home(dir.path());
    let build = Build {
        development: true,
        allow_dev_migrate: true,
    };
    Store::open_as(&home, build).unwrap();
    assert_eq!(version(&home.join(DB_FILE)), current());
    assert_eq!(backups(&home).len(), 1);
    let migrated = origin(&home).migrated.unwrap();
    assert_eq!(migrated.build, "development");
    // The flag is per run: the home still was not created by a development build.
    assert!(origin(&home).created.is_none());
}

#[test]
fn a_development_build_migrates_a_home_a_development_build_created() {
    // A new home records the build that created it.
    let dir = tempfile::tempdir().unwrap();
    let fresh = dir.path().join("fresh");
    Store::open(&fresh).unwrap();
    let created = origin(&fresh).created.expect("creation is recorded");
    assert_eq!(
        (created.build.as_str(), created.schema),
        ("development", current())
    );
    assert!(
        backups(&fresh).is_empty(),
        "nothing to back up in a new home"
    );

    // An older home with that record upgrades with no flag.
    let home = older_home(dir.path());
    record(&home, "development");
    Store::open(&home).unwrap();
    assert_eq!(version(&home.join(DB_FILE)), current());
    assert_eq!(backups(&home).len(), 1);
    assert_eq!(origin(&home).created.unwrap().build, "development");
}

#[test]
fn a_schema_this_build_does_not_know_is_refused_and_left_untouched() {
    // Same version number, different tables: another build's schema.
    let dir = tempfile::tempdir().unwrap();
    let home = older_home(dir.path());
    let db = home.join(DB_FILE);
    Connection::open(&db)
        .unwrap()
        .execute_batch("ALTER TABLE agents ADD COLUMN stray TEXT")
        .unwrap();
    let before = files(&home);
    let err = Store::open_as(&home, RELEASE).err().expect("refused");
    assert_eq!(err.code(), "invalid", "{err}");
    let text = err.to_string();
    assert!(
        text.contains("does not match") && text.contains("another doctor"),
        "{text}"
    );
    assert_eq!(files(&home), before);
    assert!(backups(&home).is_empty());

    // A negative version is no schema of ours either.
    let dir = tempfile::tempdir().unwrap();
    let home = older_home(dir.path());
    let db = home.join(DB_FILE);
    Connection::open(&db)
        .unwrap()
        .pragma_update(None, "user_version", -1)
        .unwrap();
    let before = files(&home);
    let err = Store::open_as(&home, RELEASE).err().expect("refused");
    assert!(err.to_string().contains("-1 is not valid"), "{err}");
    assert_eq!(files(&home), before);
}

#[test]
fn a_current_database_with_another_builds_tables_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    drop(Store::open(&home).unwrap());
    Connection::open(home.join(DB_FILE))
        .unwrap()
        .execute_batch("ALTER TABLE agents ADD COLUMN stray TEXT")
        .unwrap();
    let err = Store::open_as(&home, RELEASE).err().expect("refused");
    assert!(err.to_string().contains("does not match"), "{err}");
}

#[test]
fn no_backup_means_no_migration() {
    let dir = tempfile::tempdir().unwrap();
    let home = older_home(dir.path());
    // The backup directory cannot be made: a file is in its place.
    std::fs::write(home.join(BACKUP_DIR), "in the way").unwrap();
    let err = Store::open_as(&home, RELEASE).err().expect("refused");
    assert!(err.to_string().contains("backup"), "{err}");
    assert_eq!(version(&home.join(DB_FILE)), current() - 1);
    assert_eq!(peers(&home.join(DB_FILE)), 1);
    assert!(!home.join(ORIGIN_FILE).exists());
}

#[test]
fn inspect_reports_what_doctor_needs() {
    let dir = tempfile::tempdir().unwrap();
    let home = older_home(dir.path());
    let wait = std::time::Duration::from_secs(2);
    let seen = Store::inspect(&home, wait).unwrap();
    assert!(seen.known, "a true older schema is known");
    assert!(seen.development_build);
    assert!(!seen.created_by_development_build);
    assert!(seen.latest_backup.is_none());

    Store::open_as(&home, RELEASE).unwrap();
    let seen = Store::inspect(&home, wait).unwrap();
    assert_eq!(seen.origin.migrated.unwrap().build, "release");
    assert_eq!(seen.latest_backup, backups(&home).pop());

    Connection::open(home.join(DB_FILE))
        .unwrap()
        .execute_batch("CREATE TABLE stray (id INTEGER)")
        .unwrap();
    assert!(!Store::inspect(&home, wait).unwrap().known);
}
