//! An older build must not touch a database a newer build has migrated: opening it is
//! refused with a clear "upgrade another" error, and the file is left exactly as it was, so
//! the newer build keeps working (the situation of a stale binary on PATH next to a newer
//! one sharing `~/.another`).

use std::path::Path;

use another_core::test_support::schema_version as version;
use another_core::*;
use rusqlite::Connection;

fn set_version(db: &Path, v: i64) {
    Connection::open(db)
        .unwrap()
        .pragma_update(None, "user_version", v)
        .unwrap();
}

/// Everything a refused open could have changed: the schema, every event and every node.
fn snapshot(db: &Path) -> (i64, Vec<String>, i64, i64) {
    let c = Connection::open(db).unwrap();
    let mut s = c
        .prepare("SELECT type || ':' || name || ':' || coalesce(sql, '') FROM sqlite_master ORDER BY name")
        .unwrap();
    let schema = s
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap();
    let events: i64 = c
        .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
        .unwrap();
    let nodes: i64 = c
        .query_row("SELECT count(*) FROM nodes", [], |r| r.get(0))
        .unwrap();
    (version(db), schema, events, nodes)
}

#[test]
fn a_database_from_a_newer_build_is_refused_and_left_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let store = Store::open(&home).unwrap();
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
    let db = home.join(DB_FILE);
    let supported = version(&db);
    set_version(&db, supported + 1);
    let before = snapshot(&db);

    let err = Store::open(&home)
        .err()
        .expect("a newer schema must be refused");
    assert_eq!(err.code(), "invalid", "{err}");
    let text = err.to_string();
    assert!(
        text.contains(&format!("schema v{}", supported + 1))
            && text.contains(&format!("supports (v{supported})"))
            && text.contains("upgrade another"),
        "{text}"
    );
    assert_eq!(snapshot(&db), before, "a refused open changed the database");

    // The newer build's data is intact: at the supported version it opens again.
    set_version(&db, supported);
    let store = Store::open(&home).unwrap();
    assert_eq!(store.agent("alice").unwrap().name, "alice");
}
