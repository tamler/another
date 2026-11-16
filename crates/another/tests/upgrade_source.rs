//! Offline upgrade refuses unknown source catalogs before creating a backup.
use another_core::{Store, DB_FILE};
use rusqlite::Connection;

#[test]
fn absent_source_catalog_is_refused_without_backup_or_source_changes() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    drop(Store::open(&home).unwrap());
    let db = Connection::open(home.join(DB_FILE)).unwrap();
    db.execute_batch("DROP TABLE admission_holds; DROP TABLE admissions; DROP TABLE admission_secret; DROP TABLE request_holds; DROP TABLE hold_waits; DROP TABLE announcements; PRAGMA user_version=7; PRAGMA journal_mode=DELETE;").unwrap();
    drop(db);
    let before = std::fs::read(home.join(DB_FILE)).unwrap();
    let backup = temp.path().join("backup");
    assert!(
        Store::upgrade_offline(&home, &backup, true).is_err(),
        "v7 has no frozen source catalog"
    );
    assert!(
        !backup.exists(),
        "unrecognized source must not publish backup metadata"
    );
    assert!(
        !std::fs::read_dir(temp.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".another-backup-")
        }),
        "no private staging directory before source validation"
    );
    assert_eq!(std::fs::read(home.join(DB_FILE)).unwrap(), before);
}
