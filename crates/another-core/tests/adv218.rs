//! adv218 (FINDINGS-218): for a newer database the normal and the quick open give the
//! identical refusal, so the CLI and the hook log agree. (Its negative-version probe is
//! left to queue item 33, which owns that fix.)

use another_core::*;

fn set_version(home: &std::path::Path, v: i64) {
    let c = rusqlite::Connection::open(home.join(DB_FILE)).unwrap();
    c.pragma_update(None, "user_version", v).unwrap();
}

#[test]
fn adv218_quick_and_full_open_say_the_same_for_newer() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    drop(Store::open(&home).unwrap());
    set_version(&home, 99);
    let full = Store::open(&home).err().unwrap().to_string();
    let quick = Store::open_quick(&home, std::time::Duration::from_millis(50))
        .err()
        .unwrap()
        .to_string();
    assert_eq!(full, quick);
}
