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
    in_build_directory: false,
    binary: None,
};
const DEVELOPMENT: Build = Build {
    development: true,
    allow_dev_migrate: false,
    in_build_directory: false,
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
        in_build_directory: false,
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
    assert!(seen.blocked);
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

// Follow-up (Q23): release builds inside a Cargo build directory, release use of a home, and
// the review's findings.

const INSTALLED: &str = "/opt/installed/bin/another";

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

/// A binary at `root/relative`. With `built`, its directory is a Cargo build directory as
/// `cargo build` leaves one: Cargo's lock file `.cargo-lock` sits beside the binary.
fn binary_at(root: &Path, relative: &str, built: bool) -> PathBuf {
    let binary = root.join(relative);
    let dir = binary.parent().unwrap();
    std::fs::create_dir_all(dir).unwrap();
    if built {
        std::fs::write(dir.join(".cargo-lock"), "").unwrap();
    }
    std::fs::write(&binary, "a binary").unwrap();
    binary
}

/// A release-profile compile still in the build directory of a worktree under `root`.
fn worktree_build(root: &Path) -> Build {
    let binary = binary_at(
        &root.join(format!("worktree-{}", ulid_like())),
        "target/release/another",
        true,
    );
    Build::release_profile(Some(binary))
}

/// A name no other call in this test process has used.
fn ulid_like() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

/// Which release-profile binaries count as development builds: those whose own directory is
/// a Cargo build directory, told by what is in it and not by what it is called.
#[test]
fn a_release_binary_in_a_cargo_build_directory_is_a_development_build() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for inside in [
        "wt/target/release/another",
        "wt/target/aarch64-apple-darwin/release/another",
        "wt/target/debug/another",
        // A target directory by another name, a custom profile, a capital letter.
        "shared-cargo-out/release/another",
        "wt2/target/dist/another",
        "wt3/Target/release/another",
    ] {
        let build = Build::release_profile(Some(binary_at(root, inside, true)));
        assert!(build.development, "{inside}");
    }
    for installed in [
        "home/.cargo/bin/another",
        "home/.local/bin/another",
        "opt/another-0.1.1-aarch64-apple-darwin/another",
        "opt/homebrew/Cellar/another/0.1.1/bin/another",
        // Directory names alone decide nothing.
        "app/target/release/another",
        "x/target/tools/release/another",
        "y/target/release/bin/another",
    ] {
        let build = Build::release_profile(Some(binary_at(root, installed, false)));
        assert!(!build.development, "{installed}");
    }
    // A path that is not there, and a build that cannot say where it is, count as ordinary
    // release builds.
    assert!(!Build::release_profile(Some(INSTALLED.into())).development);
    assert!(!Build::release_profile(None).development);

    // This test binary: a debug build, with its own path fully resolved.
    let this = Build::current();
    assert!(this.development);
    let exe = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
    assert_eq!(this.binary, Some(exe));
}

/// O1: a release-profile build of a branch, run from a worktree against the installed
/// binary's home, is refused like any development build.
#[test]
fn a_release_build_in_a_worktree_refuses_the_installed_home() {
    let dir = tempfile::tempdir().unwrap();
    let home = installed_home(dir.path());
    let before = files(&home);
    let worktree = worktree_build(dir.path());
    let binary = worktree.binary.clone().unwrap().display().to_string();
    let err = Store::open_as(&home, worktree.clone())
        .err()
        .expect("a build still in its build directory");
    assert_eq!(err.code(), "invalid", "{err}");
    let text = err.to_string();
    // The message names the binary, says why it counts as a development build, and gives
    // the ways out for someone whose install this is: copy it out, install it, or the flag.
    assert!(
        text.contains("development build")
            && text.contains(&binary)
            && text.contains("Cargo build directory")
            && text.contains("copy this binary out")
            && text.contains("cargo install --locked --path crates/another")
            && text.contains("--dev-migrate"),
        "{text}"
    );
    assert!(!text.contains("use the installed another"), "{text}");
    assert_eq!(files(&home), before, "a refused open wrote to the home");
    assert!(backups(&home).is_empty());
    let wait = std::time::Duration::from_secs(2);
    assert!(Store::inspect_as(&home, wait, &worktree).unwrap().blocked);

    // With the flag it migrates, after a backup, and its stamp says what it was.
    let flagged = Build {
        allow_dev_migrate: true,
        ..worktree
    };
    Store::open_as(&home, flagged).unwrap();
    assert_eq!(version(&home.join(DB_FILE)), current());
    assert_eq!(backups(&home).len(), 1);
    let migrated = origin(&home).migrated.unwrap();
    assert_eq!(migrated.build, "development");
    assert_eq!(migrated.binary, Some(binary));
}

/// An installed release build migrates with no flag wherever it is: the same path, a new
/// version directory, another prefix, or a path it cannot resolve. The recorded path is for
/// doctor to show and decides nothing.
#[test]
fn an_installed_release_build_migrates_from_any_path() {
    for path in [
        Some(INSTALLED),
        Some("/opt/another-0.1.2-aarch64-apple-darwin/another"),
        Some("/opt/homebrew/Cellar/another/0.1.2/bin/another"),
        None,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let home = installed_home(dir.path());
        let build = Build::release_profile(path.map(PathBuf::from));
        let wait = std::time::Duration::from_secs(2);
        assert!(
            !Store::inspect_as(&home, wait, &build).unwrap().blocked,
            "{path:?}"
        );
        Store::open_as(&home, build).unwrap();
        assert_eq!(version(&home.join(DB_FILE)), current(), "{path:?}");
        let migrated = origin(&home).migrated.unwrap();
        assert_eq!(migrated.build, "release");
        assert_eq!(migrated.binary.as_deref(), path);
    }
}

/// P2-3: a home a development build made, then used by a release build with no migration
/// pending. That use is recorded, and it ends the development builds' free pass.
#[test]
fn release_use_of_a_development_home_is_recorded_and_ends_its_free_pass() {
    let dir = tempfile::tempdir().unwrap();
    let used = dir.path().join("used");
    drop(Store::open(&used).unwrap());
    let created = origin(&used).created.unwrap();
    assert_eq!(created.build, "development");
    assert!(created.binary.is_some(), "the creating binary is on record");

    drop(Store::open_as(&used, release_at(INSTALLED)).unwrap());
    let adopted = origin(&used).adopted.expect("release use is recorded");
    assert_eq!(adopted.build, "release");

    // The same record on a home with a migration pending.
    let home = older_home(dir.path());
    std::fs::copy(used.join(ORIGIN_FILE), home.join(ORIGIN_FILE)).unwrap();
    let before = files(&home);
    let err = Store::open_as(&home, DEVELOPMENT)
        .err()
        .expect("a release build has used this home");
    assert!(err.to_string().contains("development build"), "{err}");
    assert_eq!(files(&home), before);
    // The release build itself is not refused.
    Store::open_as(&home, release_at(INSTALLED)).unwrap();
}

/// One run with `--dev-migrate` does not make development builds free afterwards.
#[test]
fn a_flagged_run_does_not_free_later_development_builds() {
    let dir = tempfile::tempdir().unwrap();
    let home = installed_home(dir.path());
    let flagged = Build {
        allow_dev_migrate: true,
        ..DEVELOPMENT
    };
    drop(Store::open_as(&home, flagged).unwrap());
    let after = origin(&home);
    assert_eq!(after.migrated.as_ref().unwrap().build, "development");
    assert_eq!(after.created.as_ref().unwrap().build, "release");

    // The next schema change: the same record on a home with a migration pending.
    let next = dir.path().join("next");
    std::fs::create_dir(&next).unwrap();
    let next = older_home(&next);
    std::fs::copy(home.join(ORIGIN_FILE), next.join(ORIGIN_FILE)).unwrap();
    assert!(Store::open_as(&next, DEVELOPMENT).is_err());
    assert!(Store::open_as(&next, worktree_build(dir.path())).is_err());
}

/// A home only development builds have touched stays free for them, flag or no flag.
#[test]
fn a_home_only_development_builds_touched_stays_free() {
    let dir = tempfile::tempdir().unwrap();
    let home = older_home(dir.path());
    record(&home, "development");
    drop(Store::open_as(&home, worktree_build(dir.path())).unwrap());
    assert!(origin(&home).adopted.is_none(), "not release use");

    let next = dir.path().join("next");
    std::fs::create_dir(&next).unwrap();
    let next = older_home(&next);
    std::fs::copy(home.join(ORIGIN_FILE), next.join(ORIGIN_FILE)).unwrap();
    Store::open_as(&next, DEVELOPMENT).unwrap();
}

/// Every home from before the record existed, the live one included.
#[test]
fn a_home_with_no_record_is_adopted_not_refused() {
    // With a migration pending: migrated, and marked as used by a release build. The mark
    // outlasts a later migration by another build, which replaces `migrated`.
    let dir = tempfile::tempdir().unwrap();
    let home = older_home(dir.path());
    Store::open_as(&home, release_at(INSTALLED)).unwrap();
    assert_eq!(version(&home.join(DB_FILE)), current());
    let after = origin(&home);
    assert_eq!(after.migrated.unwrap().binary.as_deref(), Some(INSTALLED));
    assert_eq!(after.adopted.expect("release use").build, "release");

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

    // The first release use stays on record, and a development build never adopts.
    Store::open_as(&home, release_at("/opt/elsewhere/another")).unwrap();
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
    ] {
        assert!(err.to_string().contains("does not match"), "{err}");
    }
    assert_eq!(files(&home), before, "a quick open writes nothing");
    let err = Store::open(&home).err().expect("normal");
    assert!(err.to_string().contains("does not match"), "{err}");
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

/// Things kept in `backups` that no build wrote. The last three look like backups but do not
/// have the exact name a build gives one.
const NOT_BACKUPS: [&str; 6] = [
    "notes.txt",
    "by-hand",
    "another-v3-copy.db.bak",
    "another-v10-before-my-experiment.db",
    "another-v4-20260101.db",
    "another-v4-20260101T000000Z-final.db",
];

fn set_mtime(path: &Path, time: std::time::SystemTime) {
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_modified(time).unwrap();
}

/// Old backups as earlier migrations left them, oldest first by the time in their names,
/// plus things that are not ours. The modification times run the other way, so only the
/// names give the right order.
fn seed_backups(home: &Path, count: usize) -> Vec<PathBuf> {
    let dir = home.join(BACKUP_DIR);
    std::fs::create_dir_all(&dir).unwrap();
    for name in NOT_BACKUPS {
        if name == "by-hand" {
            std::fs::create_dir(dir.join(name)).unwrap();
        } else {
            std::fs::write(dir.join(name), "mine").unwrap();
        }
    }
    (0..count)
        .map(|n| {
            let path = dir.join(format!("another-v{}-2026010{}T000000Z.db", n + 1, n + 1));
            std::fs::write(&path, "old backup").unwrap();
            let age = std::time::Duration::from_secs(86_400 * (n as u64 + 1));
            set_mtime(&path, std::time::SystemTime::now() - age);
            path
        })
        .collect()
}

/// The files in `backups` that were not seeded by hand: the backups a build wrote or kept.
fn backup_files(home: &Path) -> Vec<PathBuf> {
    backups(home)
        .into_iter()
        .filter(|p| {
            let name = p.file_name().unwrap().to_string_lossy();
            !NOT_BACKUPS.contains(&name.as_ref())
        })
        .collect()
}

fn assert_hand_made_files_stay(home: &Path) {
    for other in NOT_BACKUPS {
        assert!(
            home.join(BACKUP_DIR).join(other).exists(),
            "{other} is not a backup a build wrote and must stay"
        );
    }
}

/// Retention: the three newest backups stay, newest by the time in the name, and only files
/// with the exact name a build writes are ever deleted.
#[test]
fn only_the_three_newest_backups_are_kept() {
    let dir = tempfile::tempdir().unwrap();
    let home = older_home(dir.path());
    let old = seed_backups(&home, 6);
    Store::open_as(&home, RELEASE).unwrap();

    let kept = backup_files(&home);
    assert_eq!(kept.len(), 3, "{kept:?}");
    assert!(old[..4].iter().all(|p| !p.exists()), "the four oldest go");
    assert!(old[4..].iter().all(|p| p.exists()), "{kept:?}");
    assert!(
        kept.iter()
            .any(|p| !old.contains(p) && version(p) == current() - 1),
        "the backup just taken is one of the three"
    );
    assert_hand_made_files_stay(&home);
}

/// P2-2: whatever the clock or the names of older files say, the backup this migration
/// wrote is never the one deleted.
#[test]
fn the_backup_just_taken_is_never_pruned() {
    let dir = tempfile::tempdir().unwrap();
    let home = older_home(dir.path());
    let backups_dir = home.join(BACKUP_DIR);
    std::fs::create_dir_all(&backups_dir).unwrap();
    // Five backups that claim to be from 2030, by name and by modification time.
    let future = std::time::SystemTime::now() + std::time::Duration::from_secs(4 * 365 * 86_400);
    let later: Vec<PathBuf> = (1..=5)
        .map(|n| {
            let path = backups_dir.join(format!("another-v{n}-2030010{n}T000000Z.db"));
            std::fs::write(&path, "from a clock set ahead").unwrap();
            set_mtime(&path, future);
            path
        })
        .collect();

    Store::open_as(&home, RELEASE).unwrap();
    assert_eq!(version(&home.join(DB_FILE)), current());
    let kept = backup_files(&home);
    let taken: Vec<&PathBuf> = kept.iter().filter(|p| !later.contains(p)).collect();
    assert_eq!(taken.len(), 1, "{kept:?}");
    assert_eq!(
        version(taken[0]),
        current() - 1,
        "a real, restorable backup"
    );
    assert_eq!(peers(taken[0]), 1);
    assert!(later.iter().all(|p| p.exists()), "{kept:?}");
}

/// A backup whose name is dated in the future (a clock that was wrong once, a file copied
/// in) is left alone: never deleted, and it does not take one of the three places.
#[test]
fn a_backup_dated_in_the_future_is_left_alone_and_takes_no_place() {
    let dir = tempfile::tempdir().unwrap();
    let home = older_home(dir.path());
    let old = seed_backups(&home, 3);
    let future = home.join(BACKUP_DIR).join("another-v9-20990101T000000Z.db");
    std::fs::write(&future, "from a clock set ahead").unwrap();

    Store::open_as(&home, RELEASE).unwrap();
    assert!(future.exists(), "a future-dated backup was deleted");
    assert!(!old[0].exists(), "the oldest real backup goes");
    assert!(
        old[1].exists() && old[2].exists(),
        "the two newest real backups stay beside the new one"
    );
    assert_eq!(backup_files(&home).len(), 4, "three, and the future one");
}

/// A migration that fails after its backup was taken rolls the database back, so that
/// backup is a copy of a database that is still there: it is removed again, and it alone.
/// Retrying a failing migration therefore does not fill the disk.
#[test]
fn a_failed_migration_removes_only_the_backup_it_took() {
    let dir = tempfile::tempdir().unwrap();
    let home = older_home(dir.path());
    let old = seed_backups(&home, 6);
    // The backup is taken, then the record cannot be written, so the migration rolls back.
    std::fs::create_dir(home.join(ORIGIN_FILE)).unwrap();
    for attempt in 1..=3 {
        let err = Store::open_as(&home, RELEASE).err().expect("no record");
        assert!(err.to_string().contains("could not write"), "{err}");
        assert_eq!(version(&home.join(DB_FILE)), current() - 1);
        assert_eq!(peers(&home.join(DB_FILE)), 1);
        assert!(old.iter().all(|p| p.exists()), "a failed migration pruned");
        assert_eq!(
            backup_files(&home),
            old,
            "attempt {attempt} left its backup behind"
        );
    }
    assert_hand_made_files_stay(&home);

    // Once the cause is gone the migration goes through and keeps its backup.
    std::fs::remove_dir(home.join(ORIGIN_FILE)).unwrap();
    Store::open_as(&home, RELEASE).unwrap();
    assert_eq!(version(&home.join(DB_FILE)), current());
    assert!(backup_files(&home)
        .iter()
        .any(|p| !old.contains(p) && version(p) == current() - 1));
}

/// Ruling 6: a database swapped for one with the same version and the same schema cookie
/// but other tables. The quick open trusts the mark; the next normal open reads the catalog
/// and refuses.
#[test]
fn a_swapped_database_is_caught_by_the_next_normal_open() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    drop(Store::open(&home).unwrap());
    let db = home.join(DB_FILE);
    let c = Connection::open(&db).unwrap();
    let cookie: i64 = c
        .pragma_query_value(None, "schema_version", |r| r.get(0))
        .unwrap();
    c.execute_batch("ALTER TABLE agents ADD COLUMN stray TEXT")
        .unwrap();
    c.pragma_update(None, "schema_version", cookie).unwrap();
    drop(c);
    assert_eq!(origin(&home).verified.unwrap().cookie, cookie);
    let busy = std::time::Duration::from_millis(50);
    Store::open_quick(&home, busy).expect("the mark still vouches for it");
    let db_before = std::fs::read(&db).unwrap();

    let err = Store::open(&home).err().expect("the normal open checks");
    assert!(err.to_string().contains("does not match"), "{err}");
    assert!(Store::open_as(&home, RELEASE).is_err());

    // The refusal takes the mark away, so quick opens and hooks stop accepting the
    // database too. Nothing else in the record changes, and the database is not written.
    let after = origin(&home);
    assert!(after.verified.is_none(), "the mark outlived the refusal");
    assert!(after.created.is_some());
    assert_eq!(std::fs::read(&db).unwrap(), db_before);
    for err in [
        Store::open_quick(&home, busy).err().expect("quick"),
        Store::open_read_only(&home, busy).err().expect("read-only"),
    ] {
        assert!(err.to_string().contains("does not match"), "{err}");
    }
}

/// P2-4: the quick open's refusal of an older schema says what a normal open would really
/// do, so the hook log does not promise a migration that will be refused.
#[test]
fn the_quick_open_says_what_a_normal_open_would_do() {
    let busy = std::time::Duration::from_millis(50);
    // A home this development build will not migrate.
    let dir = tempfile::tempdir().unwrap();
    let home = older_home(dir.path());
    let text = Store::open_quick(&home, busy).err().unwrap().to_string();
    assert!(
        text.contains("development build") && text.contains("--dev-migrate"),
        "{text}"
    );
    assert!(!text.contains("a normal open migrates it"), "{text}");
    assert_eq!(
        Store::refusal(&home).as_deref(),
        Some(text.as_str()),
        "hooks log the same reason"
    );

    // A home it will migrate: the old advice is true and stays.
    record(&home, "development");
    let text = Store::open_quick(&home, busy).err().unwrap().to_string();
    assert!(text.contains("a normal open migrates it"), "{text}");
    assert_eq!(Store::refusal(&home), None);
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

fn write_file(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn row_counts(db: &Connection) -> Vec<(String, i64)> {
    let names: Vec<String> = db
        .prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    names
        .into_iter()
        .map(|n| {
            let count = db
                .query_row(&format!("SELECT count(*) FROM \"{n}\""), [], |r| r.get(0))
                .unwrap();
            (n, count)
        })
        .collect()
}

/// A9 (D92, schema v13). A home from the build before holds were scoped to a checkout: a
/// live hold in worktree A, a wait in worktree B recorded against it while holds still
/// reached across worktrees, and a wait in A's own checkout against the same hold.
#[test]
fn upgrade_drops_cross_checkout_blocks() {
    // The schema this migration made. Later migrations follow it.
    const SCOPED: i64 = 13;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let main = root.join("shop");
    write_file(
        &main.join(".git/config"),
        "[remote \"origin\"]\n\turl = git@example.com:me/shop.git\n",
    );
    let worktree = |name: &str| -> PathBuf {
        let wt = root.join(name);
        let wt_git = main.join(".git/worktrees").join(name);
        write_file(&wt.join(".git"), &format!("gitdir: {}\n", wt_git.display()));
        write_file(&wt_git.join("commondir"), "../..\n");
        std::fs::create_dir_all(wt.join("src")).unwrap();
        wt
    };
    let (a, b) = (worktree("wt-a"), worktree("wt-b"));
    let home = root.join("home");
    let db_path = home.join(DB_FILE);
    let path = |p: &Path| p.to_string_lossy().into_owned();

    let store = Store::open_as(&home, RELEASE).unwrap();
    for (name, program) in [
        ("alice", Program::Claude),
        ("bob", Program::Codex),
        ("carol", Program::Grok),
    ] {
        store
            .register_agent(RegisterAgent {
                name: name.into(),
                program,
                project: None,
                model: None,
                description: None,
            })
            .unwrap();
    }
    let [pa, pb, pa_src] = [a.clone(), b.clone(), a.join("src")]
        .map(|dir| store.ensure_project("test", &path(&dir)).unwrap());
    fn edit(store: &Store, agent: &str, project: &Project, file: &str) -> EditGuard {
        store
            .guard_edit(agent, &project.abs_path, &[file.into()], "auto")
            .unwrap()
    }
    let held = edit(&store, "alice", &pa, "src/a.rs").held.remove(0);
    // The wait in A's own checkout is a real one.
    assert_eq!(edit(&store, "carol", &pa, "src/a.rs").conflicts.len(), 1);
    // Mail, a request and a queue item, to find again after the upgrade.
    store
        .send_message(SendMessage {
            from: "bob".into(),
            to: vec!["alice".into()],
            subject: "before the upgrade".into(),
            body: "body".into(),
            request: Some(NewRequest::default()),
            ..Default::default()
        })
        .unwrap();
    store
        .queue_propose(
            &QueueCaller::agent("bob"),
            ProposeItem {
                title: "before the upgrade".into(),
                ..Default::default()
            },
        )
        .unwrap();
    drop(store);

    // Take the home back one schema: no checkout column, the older key rules, and the wait
    // from worktree B that the build of that schema recorded against the hold in A.
    let db = Connection::open(&db_path).unwrap();
    db.execute_batch(
        "DROP TABLE lease_projections;
        DROP TABLE actor_operations;
        DROP TABLE contract_receipts;
        DROP TABLE right_leases;
        DROP TABLE contract_actors;
        DROP TABLE session_scopes;
        DROP TABLE contract_participants;
        ALTER TABLE project_repos DROP COLUMN checkout",
    )
    .unwrap();
    db.pragma_update(None, "user_version", SCOPED - 1).unwrap();
    let current_keys: i64 = db
        .query_row(
            "SELECT count(*) FROM project_repos WHERE repo_key LIKE 'v4:%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(current_keys, 3, "the three projects are linked");
    // Worktree A's row as the older build wrote it. The other two keep the current key
    // version: a row the version raise alone would never recompute.
    db.execute(
        "UPDATE project_repos SET repo_key = 'v2:' || substr(repo_key, 4) WHERE project_id = ?1",
        [&pa.project_id],
    )
    .unwrap();
    db.execute(
        "INSERT INTO hold_waits (agent_id, project_id, path, blockers, holder_id, retry_at,
           backoff_seconds)
         SELECT (SELECT agent_id FROM agents WHERE name = 'bob'), ?1, path, blockers, holder_id,
           retry_at, backoff_seconds FROM hold_waits",
        [&pb.project_id],
    )
    .unwrap();
    let hold_row = |db: &Connection| -> (String, String, i64, i64, Option<i64>) {
        db.query_row(
            "SELECT project_id, pattern, ttl_seconds, expires_at, released_at FROM holds
             WHERE hold_id = ?1",
            [&held.hold_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap()
    };
    let blockers = |db: &Connection| -> Vec<(String, String)> {
        db.prepare(
            "SELECT a.name, w.blockers FROM hold_waits w JOIN agents a USING (agent_id)
             ORDER BY a.name",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    };
    let one = format!("[\"{}\"]", held.hold_id);
    assert_eq!(
        blockers(&db),
        [("bob".into(), one.clone()), ("carol".into(), one.clone())]
    );
    let (hold_before, rows_before) = (hold_row(&db), row_counts(&db));
    drop(db);

    // The upgrade: a backup of the old schema first, then one additive step.
    let store = Store::open_as(&home, RELEASE).unwrap();
    let db = Connection::open(&db_path).unwrap();
    assert_eq!(version(&db_path), current());
    let saved = backups(&home);
    assert_eq!(saved.len(), 1, "{saved:?}");
    assert_eq!(version(&saved[0]), SCOPED - 1);
    for (name, count) in &rows_before {
        let expected = if name == "hold_waits" { 0 } else { *count };
        assert_eq!(
            row_counts(&db).iter().find(|(n, _)| n == name).unwrap().1,
            expected,
            "durable rows survive; legacy waits are disposed at contract cutover"
        );
    }
    assert_eq!(hold_before.4, None);
    let migrated_hold = hold_row(&db);
    assert_eq!(
        (
            migrated_hold.0.clone(),
            migrated_hold.1.clone(),
            migrated_hold.2,
            migrated_hold.3
        ),
        (
            hold_before.0.clone(),
            hold_before.1.clone(),
            hold_before.2,
            hold_before.3
        )
    );
    assert!(
        migrated_hold.4.is_some(),
        "legacy ownership is never adopted by name"
    );
    assert!(blockers(&db).is_empty());
    let checkouts = |db: &Connection| -> Vec<(String, String)> {
        db.prepare(
            "SELECT p.slug, r.checkout FROM project_repos r JOIN projects p USING (project_id)
             ORDER BY p.slug",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    };
    assert!(
        checkouts(&db).iter().all(|(_, c)| c.is_empty()),
        "nothing is computed by the migration itself"
    );

    // A fresh checkout follows the checkout rule immediately. Cutover does not
    // send notices for disposed legacy waits.
    let held = edit(&store, "alice", &pa, "src/a.rs").held.remove(0);
    assert_eq!(edit(&store, "carol", &pa, "src/a.rs").conflicts.len(), 1);
    let one = format!("[\"{}\"]", held.hold_id);
    let unread = |agent: &str| {
        store
            .inbox(
                agent,
                InboxQuery {
                    unread_only: true,
                    urgent_only: false,
                    limit: 10,
                },
            )
            .unwrap()
            .messages
    };
    let pings = |db: &Connection| -> i64 {
        db.query_row(
            "SELECT count(*) FROM messages WHERE subject LIKE 'Hold free:%'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(pings(&db), 0, "opening the home sends nothing");
    store.sweep().unwrap();
    assert_eq!(pings(&db), 0);
    let mail = unread("bob");
    assert!(mail.is_empty(), "{mail:?}");
    assert!(unread("carol").is_empty());
    assert_eq!(blockers(&db), [("carol".into(), one)]);
    store.sweep().unwrap();
    assert!(unread("bob").is_empty());
    assert_eq!(hold_row(&db), migrated_hold);
    // The new rule from the first command: bob edits in B, carol is still stopped in A.
    assert!(edit(&store, "bob", &pb, "src/a.rs").conflicts.is_empty());
    assert_eq!(edit(&store, "carol", &pa, "src/a.rs").conflicts.len(), 1);

    // The checkout is filled on first use. Worktree A's row had the older key version; the
    // row of the project at A's `src` already carries the current one with an empty
    // checkout, and it still blocks through the top-level project, in both directions.
    let a_real = path(&resolve_existing(&a));
    let b_real = path(&resolve_existing(&b));
    let filled = checkouts(&db);
    assert!(
        filled.contains(&("wt-a".into(), a_real.clone())),
        "{filled:?}"
    );
    assert!(filled.contains(&("wt-b".into(), b_real)), "{filled:?}");
    let src_row = |db: &Connection| -> (String, String) {
        db.query_row(
            "SELECT repo_key, checkout FROM project_repos WHERE project_id = ?1",
            [&pa_src.project_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    };
    let (key, checkout) = src_row(&db);
    assert!(key.starts_with("v4:"), "{key}");
    assert_eq!(checkout, "", "not used yet");
    let stopped = edit(&store, "bob", &pa_src, "a.rs");
    assert_eq!(stopped.conflicts.len(), 1, "src/a.rs is alice's, in wt-a");
    assert_eq!(stopped.conflicts[0].held.hold_id, held.hold_id);
    assert_eq!(src_row(&db), (key.clone(), a_real.clone()));
    // The reverse: the holder sits in the project whose row has no checkout yet.
    assert!(edit(&store, "bob", &pa_src, "lib.rs").conflicts.is_empty());
    db.execute(
        "UPDATE project_repos SET checkout = '' WHERE project_id = ?1",
        [&pa_src.project_id],
    )
    .unwrap();
    let stopped = edit(&store, "carol", &pa, "src/lib.rs");
    assert_eq!(stopped.conflicts.len(), 1);
    assert_eq!(stopped.conflicts[0].held.agent, "bob");
    assert_eq!(src_row(&db), (key, a_real));
}
