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
    binary: None,
};
const DEVELOPMENT: Build = Build {
    development: true,
    allow_dev_migrate: false,
    binary: None,
};

/// A release build whose executable is at `path`.
fn release_at(path: &str) -> Build {
    Build {
        binary: Some(PathBuf::from(path)),
        ..RELEASE
    }
}

fn journal_mode(db: &Path) -> String {
    Connection::open(db)
        .unwrap()
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap()
}

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
        binary: None,
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
    assert_eq!(seen.blocked, Some(Blocked::DevelopmentBuild));
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

// Follow-up (Q23): the binary-path rule, the last migration, and the review's findings.

const INSTALLED: &str = "/opt/installed/bin/another";
const WORKTREE: &str = "/src/worktree/target/release/another";

/// An older home that the release binary at `INSTALLED` created.
fn installed_home(dir: &Path) -> PathBuf {
    let home = older_home(dir);
    std::fs::write(
        home.join(ORIGIN_FILE),
        format!(
            r#"{{"created":{{"version":"0.1.0","build":"release","schema":{},"at":"2026-11-01T00:00:00.000Z","binary":"{INSTALLED}"}}}}"#,
            current() - 1
        ),
    )
    .unwrap();
    home
}

/// O1: a release-profile build of a branch, run from a worktree against the installed
/// binary's home.
#[test]
fn a_release_binary_at_another_path_refuses_and_names_both_paths() {
    let dir = tempfile::tempdir().unwrap();
    let home = installed_home(dir.path());
    let before = files(&home);
    let err = Store::open_as(&home, release_at(WORKTREE))
        .err()
        .expect("another binary's home");
    assert_eq!(err.code(), "invalid", "{err}");
    let text = err.to_string();
    assert!(
        text.contains(INSTALLED) && text.contains(WORKTREE) && text.contains("--dev-migrate"),
        "{text}"
    );
    assert_eq!(files(&home), before, "a refused open wrote to the home");
    assert!(backups(&home).is_empty());

    // Doctor is told the same thing.
    let seen = Store::inspect_as(
        &home,
        std::time::Duration::from_secs(2),
        &release_at(WORKTREE),
    )
    .unwrap();
    assert_eq!(
        seen.blocked,
        Some(Blocked::OtherBinary {
            recorded: INSTALLED.into(),
            this: WORKTREE.into()
        })
    );

    // With the flag it migrates, after a backup, and the home is now this binary's.
    let flagged = Build {
        allow_dev_migrate: true,
        ..release_at(WORKTREE)
    };
    Store::open_as(&home, flagged).unwrap();
    assert_eq!(version(&home.join(DB_FILE)), current());
    assert_eq!(backups(&home).len(), 1);
    assert_eq!(
        origin(&home).migrated.unwrap().binary.as_deref(),
        Some(WORKTREE)
    );
}

/// An upgrade in place (`cargo install`, or a new file at the old path) needs no flag.
#[test]
fn the_same_path_migrates_with_no_flag() {
    let dir = tempfile::tempdir().unwrap();
    let home = installed_home(dir.path());
    Store::open_as(&home, release_at(INSTALLED)).unwrap();
    assert_eq!(version(&home.join(DB_FILE)), current());
    let migrated = origin(&home).migrated.unwrap();
    assert_eq!(migrated.binary.as_deref(), Some(INSTALLED));
    assert_eq!(migrated.build, "release");
}

/// Every home from before the path was recorded, the live one included.
#[test]
fn a_home_with_no_record_is_adopted_not_refused() {
    // With a migration pending: migrated, and recorded as this binary's.
    let dir = tempfile::tempdir().unwrap();
    let home = older_home(dir.path());
    Store::open_as(&home, release_at(INSTALLED)).unwrap();
    assert_eq!(version(&home.join(DB_FILE)), current());
    assert_eq!(
        origin(&home).migrated.unwrap().binary.as_deref(),
        Some(INSTALLED)
    );

    // At the current schema: the first release build to open it adopts it.
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    drop(Store::open(&home).unwrap());
    std::fs::remove_file(home.join(ORIGIN_FILE)).unwrap();
    let db_before = std::fs::read(home.join(DB_FILE)).unwrap();
    Store::open_as(&home, release_at(INSTALLED)).unwrap();
    let adopted = origin(&home).adopted.expect("adopted on first contact");
    assert_eq!(adopted.binary.as_deref(), Some(INSTALLED));
    assert_eq!(adopted.build, "release");
    assert_eq!(std::fs::read(home.join(DB_FILE)).unwrap(), db_before);

    // A later binary elsewhere does not take it over, and a development build never adopts.
    Store::open_as(&home, release_at(WORKTREE)).unwrap();
    assert_eq!(
        origin(&home).adopted.unwrap().binary.as_deref(),
        Some(INSTALLED)
    );
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    drop(Store::open(&home).unwrap());
    std::fs::remove_file(home.join(ORIGIN_FILE)).unwrap();
    Store::open_as(&home, DEVELOPMENT).unwrap();
    assert!(origin(&home).adopted.is_none());
}

/// O2: a home made by a development build and later taken over by a release build.
#[test]
fn a_debug_build_does_not_migrate_a_home_a_release_build_has_touched() {
    for touched in ["migrated", "adopted"] {
        let dir = tempfile::tempdir().unwrap();
        let home = older_home(dir.path());
        let stamp = |build: &str| {
            format!(
                r#"{{"version":"0.1.0","build":"{build}","schema":{},"at":"2026-11-01T00:00:00.000Z"}}"#,
                current() - 1
            )
        };
        std::fs::write(
            home.join(ORIGIN_FILE),
            format!(
                r#"{{"created":{},"{touched}":{}}}"#,
                stamp("development"),
                stamp("release")
            ),
        )
        .unwrap();
        let before = files(&home);
        let err = Store::open_as(&home, DEVELOPMENT)
            .err()
            .expect("a release build has had this home");
        assert!(err.to_string().contains("development build"), "{err}");
        assert_eq!(files(&home), before);
    }
}

/// P3-1: a database not in WAL mode keeps its journal mode, and its bytes, when refused.
#[test]
fn a_refused_open_does_not_switch_the_journal_mode() {
    let dir = tempfile::tempdir().unwrap();
    let refusals: [(&str, i64, Build); 3] = [
        ("development build", current() - 1, DEVELOPMENT),
        ("newer", current() + 1, RELEASE),
        ("does not match", 0, RELEASE),
    ];
    for (n, (why, schema, build)) in refusals.into_iter().enumerate() {
        let home = dir.path().join(format!("home{n}"));
        std::fs::create_dir_all(&home).unwrap();
        let db = home.join(DB_FILE);
        let c = Connection::open(&db).unwrap();
        apply_schema(&c, current() - 1).unwrap();
        c.pragma_update(None, "user_version", schema).unwrap();
        drop(c);
        assert_eq!(journal_mode(&db), "delete");
        let before = std::fs::read(&db).unwrap();
        let err = Store::open_as(&home, build).err().expect(why);
        assert!(err.to_string().contains(why), "{err}");
        assert_eq!(
            std::fs::read(&db).unwrap(),
            before,
            "{why}: the file changed"
        );
        assert_eq!(journal_mode(&db), "delete", "{why}");
    }
}

/// P3-4: the record cannot be written, so nothing is migrated and nothing is left behind.
#[test]
fn a_failed_record_write_names_the_file_and_leaves_no_temp_file() {
    let dir = tempfile::tempdir().unwrap();
    let home = older_home(dir.path());
    std::fs::create_dir(home.join(ORIGIN_FILE)).unwrap();
    let err = Store::open_as(&home, RELEASE).err().expect("no record");
    let text = err.to_string();
    assert!(
        text.contains(&home.join(ORIGIN_FILE).display().to_string()),
        "{text}"
    );
    assert_eq!(version(&home.join(DB_FILE)), current() - 1);
    let stray: Vec<String> = std::fs::read_dir(&home)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    assert!(stray.is_empty(), "{stray:?}");

    // The backup failure names its directory too.
    let dir = tempfile::tempdir().unwrap();
    let home = older_home(dir.path());
    std::fs::write(home.join(BACKUP_DIR), "in the way").unwrap();
    let err = Store::open_as(&home, RELEASE).err().expect("no backup");
    assert!(
        err.to_string()
            .contains(&home.join(BACKUP_DIR).display().to_string()),
        "{err}"
    );
}

/// P3-5: `board` and the tool hooks use the quick open.
#[test]
fn quick_opens_refuse_a_schema_the_normal_open_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    drop(Store::open(&home).unwrap());
    let busy = std::time::Duration::from_millis(50);
    Store::open_quick(&home, busy).unwrap();
    Store::open_read_only(&home, busy).unwrap();
    Connection::open(home.join(DB_FILE))
        .unwrap()
        .execute_batch("ALTER TABLE agents ADD COLUMN stray TEXT")
        .unwrap();
    let before = files(&home);
    for err in [
        Store::open_quick(&home, busy).err().expect("quick"),
        Store::open_read_only(&home, busy).err().expect("read-only"),
        Store::open(&home).err().expect("normal"),
    ] {
        assert!(err.to_string().contains("does not match"), "{err}");
    }
    assert_eq!(files(&home), before);
}

/// The table check is remembered against SQLite's schema cookie, so opens after the first
/// skip it; any change to the schema moves the cookie and the check runs again.
#[test]
fn a_verified_schema_is_remembered() {
    let cookie = |db: &Path| -> i64 {
        Connection::open(db)
            .unwrap()
            .pragma_query_value(None, "schema_version", |r| r.get(0))
            .unwrap()
    };
    let busy = std::time::Duration::from_millis(50);
    // A new home and a migrated home both carry the mark for the schema they ended at.
    let dir = tempfile::tempdir().unwrap();
    let fresh = dir.path().join("fresh");
    drop(Store::open(&fresh).unwrap());
    let older = older_home(dir.path());
    drop(Store::open_as(&older, RELEASE).unwrap());
    for home in [&fresh, &older] {
        let verified = origin(home).verified.expect("verified");
        assert_eq!(verified.schema, current());
        assert_eq!(verified.cookie, cookie(&home.join(DB_FILE)), "{home:?}");
    }

    // A home with no mark gets one from the first normal open, and no other field moves.
    let mut stripped = origin(&fresh);
    stripped.verified = None;
    std::fs::write(
        fresh.join(ORIGIN_FILE),
        serde_json::to_vec(&stripped).unwrap(),
    )
    .unwrap();
    Store::open_quick(&fresh, busy).unwrap();
    assert!(
        origin(&fresh).verified.is_none(),
        "a quick open writes nothing"
    );
    drop(Store::open(&fresh).unwrap());
    let marked = origin(&fresh);
    assert!(marked.verified.is_some());
    assert_eq!(marked.created, stripped.created);

    // A mark carrying another build's digest is not trusted.
    let mut forged = origin(&fresh);
    Connection::open(fresh.join(DB_FILE))
        .unwrap()
        .execute_batch("ALTER TABLE agents ADD COLUMN stray TEXT")
        .unwrap();
    let mark = forged.verified.as_mut().unwrap();
    mark.cookie = cookie(&fresh.join(DB_FILE));
    mark.digest = "the digest of some other build".into();
    std::fs::write(
        fresh.join(ORIGIN_FILE),
        serde_json::to_vec(&forged).unwrap(),
    )
    .unwrap();
    assert!(Store::open(&fresh).is_err());
    assert!(Store::open_quick(&fresh, busy).is_err());
}

/// P3-7: `backups` as a symlink is neither followed nor chmodded.
#[cfg(unix)]
#[test]
fn a_symlinked_backup_directory_is_not_followed() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let home = older_home(dir.path());
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(&elsewhere, home.join(BACKUP_DIR)).unwrap();

    let err = Store::open_as(&home, RELEASE).err().expect("refused");
    assert!(err.to_string().contains("symbolic link"), "{err}");
    assert_eq!(version(&home.join(DB_FILE)), current() - 1);
    assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
    let mode = std::fs::metadata(&elsewhere).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755, "the link's target was chmodded");
}

/// Old backups as earlier migrations left them, oldest first, plus things that are not ours.
fn seed_backups(home: &Path, count: usize) -> Vec<PathBuf> {
    let dir = home.join(BACKUP_DIR);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("notes.txt"), "mine").unwrap();
    std::fs::create_dir(dir.join("by-hand")).unwrap();
    std::fs::write(dir.join("another-v3-copy.db.bak"), "mine").unwrap();
    (0..count)
        .map(|n| {
            let path = dir.join(format!("another-v{}-2026010{}T000000Z.db", n + 1, n + 1));
            std::fs::write(&path, "old backup").unwrap();
            let age = std::time::Duration::from_secs(86_400 * (count - n) as u64 + 3_600);
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(std::time::SystemTime::now() - age)
                .unwrap();
            path
        })
        .collect()
}

fn backup_files(home: &Path) -> Vec<PathBuf> {
    backups(home)
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "db"))
        .collect()
}

/// Retention: the five newest backups stay, and only backup files are ever deleted.
#[test]
fn only_the_five_newest_backups_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    let home = older_home(dir.path());
    let old = seed_backups(&home, 6);
    Store::open_as(&home, RELEASE).unwrap();

    let kept = backup_files(&home);
    assert_eq!(kept.len(), 5, "{kept:?}");
    assert!(!old[0].exists() && !old[1].exists(), "the two oldest go");
    assert!(old[2..].iter().all(|p| p.exists()), "{kept:?}");
    assert!(
        kept.iter()
            .any(|p| !old.contains(p) && version(p) == current() - 1),
        "the backup just taken is one of the five"
    );
    for other in ["notes.txt", "by-hand", "another-v3-copy.db.bak"] {
        assert!(
            home.join(BACKUP_DIR).join(other).exists(),
            "{other} is not a backup and must stay"
        );
    }
}

#[test]
fn a_failed_migration_deletes_no_backup() {
    let dir = tempfile::tempdir().unwrap();
    let home = older_home(dir.path());
    let old = seed_backups(&home, 6);
    // The backup is taken, then the record cannot be written, so the migration rolls back.
    std::fs::create_dir(home.join(ORIGIN_FILE)).unwrap();
    assert!(Store::open_as(&home, RELEASE).is_err());
    assert_eq!(version(&home.join(DB_FILE)), current() - 1);
    assert!(old.iter().all(|p| p.exists()), "a failed migration pruned");
    assert_eq!(
        backup_files(&home).len(),
        7,
        "the six old ones and the one just taken"
    );
}

/// P3-7: the refusal says where the schema differs.
#[test]
fn a_mismatch_names_the_tables_that_differ() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    drop(Store::open(&home).unwrap());
    Connection::open(home.join(DB_FILE))
        .unwrap()
        .execute_batch(
            "ALTER TABLE agents ADD COLUMN stray TEXT;
             CREATE TABLE extra (id INTEGER);",
        )
        .unwrap();
    let text = Store::open(&home).err().expect("refused").to_string();
    assert!(
        text.contains("a different build changed it") && text.contains("agents, extra"),
        "{text}"
    );
    let seen = Store::inspect(&home, std::time::Duration::from_secs(2)).unwrap();
    assert_eq!(seen.differs, ["agents", "extra"]);
}
