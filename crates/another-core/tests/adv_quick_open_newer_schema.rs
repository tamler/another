//! Adversarial (main): the quick open used by tool hooks and `another board` must give the
//! same advice as a normal open for a database a newer build migrated. Today it says "a
//! normal open migrates it", but a normal open refuses a newer schema.

use another_core::*;

#[test]
fn gap_a_quick_open_of_a_newer_schema_says_to_upgrade() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    drop(Store::open(&home).unwrap());
    let db = home.join(DB_FILE);
    let c = rusqlite::Connection::open(&db).unwrap();
    let v: i64 = c
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    c.pragma_update(None, "user_version", v + 1).unwrap();
    drop(c);

    let full = Store::open(&home).err().unwrap().to_string();
    assert!(full.contains("upgrade another"), "{full}");
    let quick = Store::open_quick(&home, std::time::Duration::from_millis(50))
        .err()
        .unwrap()
        .to_string();
    assert!(
        quick.contains("upgrade another") && !quick.contains("a normal open migrates it"),
        "quick open advises a migration a normal open refuses: {quick}"
    );
}
