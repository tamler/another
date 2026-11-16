//! Databases from before node keys existed: on open, the store creates the local node's key
//! and signs the events this node authored, and only those. A foreign node's unsigned event
//! must stay unsigned: signing it would vouch, with this node's key, for words it never
//! wrote.

use std::path::Path;

use another_core::*;
use rusqlite::{params, Connection};

const FOREIGN: &str = "01FOREIGNFOREIGNFOREIGNFOR";

struct Row {
    event_id: String,
    node_id: String,
    ts: i64,
    actor: String,
    kind: String,
    payload: String,
    sig: Option<String>,
}

fn rows(db: &Path) -> Vec<Row> {
    let c = Connection::open(db).unwrap();
    let mut s = c
        .prepare("SELECT event_id, node_id, ts, actor, kind, payload, sig FROM events ORDER BY seq")
        .unwrap();
    s.query_map([], |r| {
        Ok(Row {
            event_id: r.get(0)?,
            node_id: r.get(1)?,
            ts: r.get(2)?,
            actor: r.get(3)?,
            kind: r.get(4)?,
            payload: r.get(5)?,
            sig: r.get(6)?,
        })
    })
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

fn local_keys(db: &Path) -> (String, Option<String>, Option<String>) {
    Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT node_id, public_key, secret_key FROM nodes WHERE is_local = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
}

/// A home whose database looks like one written before node keys: no local key, no event
/// signatures, plus one unsigned event from another node.
fn legacy_home(dir: &Path) -> std::path::PathBuf {
    let home = dir.join("home");
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
    let c = Connection::open(&db).unwrap();
    c.execute(
        "UPDATE nodes SET public_key = NULL, secret_key = NULL WHERE is_local = 1",
        [],
    )
    .unwrap();
    c.execute("UPDATE events SET sig = NULL", []).unwrap();
    c.execute(
        "INSERT INTO nodes (node_id, display_name, role, is_local, created_at) VALUES (?1, 'other', 'peer', 0, 0)",
        [FOREIGN],
    )
    .unwrap();
    c.execute(
        "INSERT INTO events (event_id, node_id, ts, actor, kind, payload, prev_id, sig)
         VALUES ('foreign-1', ?1, 1, 'mallory', 'agent.registered', '{}', NULL, NULL)",
        params![FOREIGN],
    )
    .unwrap();
    db
}

#[test]
fn opening_a_keyless_database_creates_a_key_and_signs_only_local_events() {
    let dir = tempfile::tempdir().unwrap();
    let db = legacy_home(dir.path());
    let (local, public, secret) = local_keys(&db);
    assert!(public.is_none() && secret.is_none());
    let before = rows(&db);
    assert!(before.iter().all(|r| r.sig.is_none()));
    let local_count = before.iter().filter(|r| r.node_id == local).count();
    assert!(local_count >= 2, "{local_count}");

    drop(Store::open(db.parent().unwrap()).unwrap());

    let (_, public, secret) = local_keys(&db);
    let public = public.expect("a public key was created");
    let secret = secret.expect("a secret key was created");
    assert_eq!(
        crypto::public_of(&crypto::decode_secret(&secret).unwrap()),
        public,
        "the stored public key belongs to the stored secret"
    );
    for r in rows(&db) {
        if r.node_id == local {
            let sig = r
                .sig
                .unwrap_or_else(|| panic!("{} was not signed", r.event_id));
            let msg =
                crypto::event_message(&r.event_id, &r.node_id, r.ts, &r.actor, &r.kind, &r.payload);
            assert!(
                crypto::verify(&public, &msg, &sig),
                "{} does not verify",
                r.event_id
            );
        } else {
            assert_eq!(r.node_id, FOREIGN);
            assert!(
                r.sig.is_none(),
                "a foreign event was signed with this node's key"
            );
        }
    }
}

/// The upgrade happens once: opening again keeps the key and every signature as they are.
#[test]
fn reopening_keeps_the_key_and_the_signatures() {
    let dir = tempfile::tempdir().unwrap();
    let db = legacy_home(dir.path());
    drop(Store::open(db.parent().unwrap()).unwrap());
    let first_keys = local_keys(&db);
    let first: Vec<_> = rows(&db).into_iter().map(|r| (r.event_id, r.sig)).collect();

    let store = Store::open(db.parent().unwrap()).unwrap();
    // New events are signed with the same key.
    store
        .register_agent(RegisterAgent {
            name: "bob".into(),
            program: Program::Codex,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    drop(store);

    assert_eq!(local_keys(&db), first_keys);
    let second: Vec<_> = rows(&db).into_iter().map(|r| (r.event_id, r.sig)).collect();
    assert_eq!(
        &second[..first.len()],
        &first[..],
        "existing signatures changed"
    );
    let (local, public, _) = first_keys;
    for r in rows(&db).into_iter().filter(|r| r.node_id == local) {
        let msg =
            crypto::event_message(&r.event_id, &r.node_id, r.ts, &r.actor, &r.kind, &r.payload);
        assert!(crypto::verify(
            public.as_deref().unwrap(),
            &msg,
            r.sig.as_deref().unwrap()
        ));
    }
}

/// adv205: a home that already has a key but holds unsigned local events gets them signed on
/// open with its existing key, not a new one.
#[test]
fn adv205_unsigned_local_events_are_signed_with_the_existing_key() {
    let dir = tempfile::tempdir().unwrap();
    let db = legacy_home(dir.path());
    drop(Store::open(db.parent().unwrap()).unwrap());
    let keys = local_keys(&db);
    Connection::open(&db)
        .unwrap()
        .execute("UPDATE events SET sig = NULL WHERE node_id = ?1", [&keys.0])
        .unwrap();

    drop(Store::open(db.parent().unwrap()).unwrap());

    assert_eq!(local_keys(&db), keys, "the existing key is kept");
    let public = keys.1.unwrap();
    for r in rows(&db).into_iter().filter(|r| r.node_id == keys.0) {
        let sig = r
            .sig
            .unwrap_or_else(|| panic!("{} was not signed", r.event_id));
        let msg =
            crypto::event_message(&r.event_id, &r.node_id, r.ts, &r.actor, &r.kind, &r.payload);
        assert!(crypto::verify(&public, &msg, &sig), "{}", r.event_id);
    }
}
