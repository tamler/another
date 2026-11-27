//! For this workspace's tests only: schema, checkout and pairing fixtures, and wire events a test
//! can forge, re-sign or replay. Signing goes through [`crate::crypto`]; nothing here is a
//! second implementation of the signed bytes.

use rusqlite::Connection;
use std::path::{Path, PathBuf};

use crate::{crypto, Clock, Perm, Store, WireEvent, DB_FILE};

/// A named test node in a caller-owned temporary home.
pub struct Node {
    pub store: Store,
    pub db: PathBuf,
}

impl Node {
    pub fn open(home: &Path, name: &str, clock: Clock) -> Self {
        let store = Store::open_with_clock(home, clock).unwrap();
        store.set_node_name(name).unwrap();
        Self {
            store,
            db: home.join(DB_FILE),
        }
    }
}

/// Pair two stores in process and return the consumed invite code for assertions.
///
/// The invite always names the fixed hub URL `https://hub.example` and a 3600-second TTL.
/// Tests that check the hub URL, expiry or a tampered handshake should build the invite
/// themselves instead of using this helper.
pub fn pair(hub: &Store, spoke: &Store, perms: &[Perm]) -> String {
    let code = hub
        .create_invite("https://hub.example", perms, 3600)
        .unwrap();
    let (invite, request) = spoke.prepare_join(&code).unwrap();
    let response = hub.accept_pairing(&request).unwrap();
    spoke.complete_join(&invite, &response).unwrap();
    code
}

/// Read a fixture database's migration version without creating a missing database.
pub fn schema_version(db: &Path) -> i64 {
    Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap()
}

/// The schema produced by this build, without creating a home on disk.
pub fn current_schema_version() -> i64 {
    static VERSION: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *VERSION.get_or_init(|| {
        let (clock, _) = crate::Clock::manual(0);
        crate::Store::open_in_memory(clock)
            .unwrap()
            .read(|conn| Ok(conn.pragma_query_value(None, "user_version", |r| r.get(0))?))
            .unwrap()
    })
}

/// Create an empty WAL home `behind` migrations before this build. Returns the
/// older version, with no build record or node rows; callers add scenario-specific data.
/// The destination must not already contain a database or build record.
pub fn older_home(home: &Path, behind: i64) -> i64 {
    let current = current_schema_version();
    assert!(
        behind > 0 && behind <= current,
        "invalid migration distance: behind={behind}, current={current}"
    );
    let version = current.checked_sub(behind).unwrap();
    assert!(!home.join(crate::DB_FILE).exists());
    assert!(!home.join(crate::ORIGIN_FILE).exists());
    std::fs::create_dir_all(home).unwrap();
    let conn = Connection::open(home.join(crate::DB_FILE)).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    crate::apply_schema(&conn, version).unwrap();
    version
}

/// A file-only checkout, with an optional origin. No Git process runs.
pub fn fake_checkout(dir: &Path, origin: Option<&str>) {
    let config = match origin {
        Some(url) => format!("[remote \"origin\"]\n\turl = {url}\n"),
        None => "[core]\n\tbare = false\n".into(),
    };
    write_fixture(&dir.join(".git/config"), &config);
}

/// A linked worktree at `at`, using an absolute gitdir and relative commondir.
/// Callers create any source directories their scenario needs.
pub fn fake_worktree(main: &Path, at: &Path) {
    let git_dir = main.join(".git/worktrees").join(at.file_name().unwrap());
    let git_dir = std::path::absolute(git_dir).unwrap();
    write_fixture(
        &at.join(".git"),
        &format!("gitdir: {}\n", git_dir.display()),
    );
    write_fixture(&git_dir.join("commondir"), "../..\n");
}

fn write_fixture(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// `ev`, signed with the secret key of the local node of the home `conn` is open on. The
/// incoming `sig` is ignored. `ev.node_id` should be that local node's id for the event to
/// verify on a peer; a test that wants a mismatch can set another one.
pub fn forge_event(conn: &Connection, mut ev: WireEvent) -> WireEvent {
    let secret: String = conn
        .query_row("SELECT secret_key FROM nodes WHERE is_local = 1", [], |r| {
            r.get(0)
        })
        .expect("the home has a local node with a secret key");
    let key = crypto::decode_secret(&secret).expect("the stored secret key decodes");
    ev.sig = crypto::sign(
        &key,
        &crypto::event_message(
            &ev.event_id,
            &ev.node_id,
            ev.ts,
            &ev.actor,
            &ev.kind,
            &ev.payload,
        ),
    );
    ev
}

/// Every event of origin `node_id` in the log `conn` is open on, oldest first, exactly as
/// stored and signed. An unsigned row has an empty `sig`.
pub fn wire_events_of(conn: &Connection, node_id: &str) -> Vec<WireEvent> {
    let mut stmt = conn
        .prepare(
            "SELECT seq, event_id, node_id, ts, actor, kind, payload, prev_id, sig
             FROM events WHERE node_id = ?1 ORDER BY seq",
        )
        .expect("the events table reads");
    stmt.query_map([node_id], |r| {
        Ok(WireEvent {
            seq: r.get(0)?,
            event_id: r.get(1)?,
            node_id: r.get(2)?,
            ts: r.get(3)?,
            actor: r.get(4)?,
            kind: r.get(5)?,
            payload: r.get(6)?,
            prev_id: r.get(7)?,
            sig: r.get::<_, Option<String>>(8)?.unwrap_or_default(),
        })
    })
    .expect("the events table reads")
    .collect::<rusqlite::Result<_>>()
    .expect("every event row maps to a wire event")
}
