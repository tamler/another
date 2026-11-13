//! Session identity safety gates through the public API: the bounded no-create opener,
//! the durable fence for identity commits, and atomic edit admission.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use another_core::{EditScope, Error, HoldFilter, Program, Store, FENCE_DIR};

struct Fixture {
    _dir: tempfile::TempDir,
    home: PathBuf,
    shop: String,
    docs: String,
}

impl Fixture {
    fn new() -> (Fixture, Store) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let shop = dir.path().join("shop");
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&shop).unwrap();
        std::fs::create_dir_all(&docs).unwrap();
        let store = Store::open(&home).unwrap();
        let fixture = Fixture {
            home,
            shop: shop.to_str().unwrap().to_owned(),
            docs: docs.to_str().unwrap().to_owned(),
            _dir: dir,
        };
        (fixture, store)
    }

    fn fence(&self, name: &str) -> PathBuf {
        let dir = self.home.join(FENCE_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, b"session.binding.v1\n").unwrap();
        path
    }
}

fn start(store: &Store, root: &str, session: &str) -> String {
    store
        .session_actor("claude", Program::Claude, root, None, session, true)
        .unwrap()
        .0
}

fn admit(
    store: &Store,
    root: &str,
    session: &str,
    groups: Vec<(String, Vec<String>)>,
) -> another_core::Result<another_core::EditAdmission> {
    store.admit_edit(
        "claude",
        Program::Claude,
        root,
        None,
        session,
        "editing (test)",
        |_| EditScope {
            groups,
            unguarded: Vec::new(),
        },
    )
}

/// Expiry of every live hold of `agent`, in milliseconds, straight from the database.
fn expiries(home: &Path, agent: &str) -> Vec<(String, i64)> {
    let db = rusqlite::Connection::open(home.join("another.db")).unwrap();
    let mut stmt = db
        .prepare(
            "SELECT h.hold_id, h.expires_at FROM holds h JOIN agents a ON a.agent_id = h.agent_id
             WHERE a.address = ?1 AND h.released_at IS NULL ORDER BY h.pattern",
        )
        .unwrap();
    let rows = stmt
        .query_map([agent], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    rows.map(Result::unwrap).collect()
}

fn held_by(store: &Store, agent: &str) -> Vec<String> {
    store
        .holds(HoldFilter {
            project: None,
            agent: Some(agent.to_owned()),
        })
        .unwrap()
        .into_iter()
        .map(|h| format!("{}:{}", h.project, h.pattern))
        .collect()
}

fn counts(home: &Path) -> (usize, usize, usize) {
    let store = Store::open_quick(home, Duration::from_millis(200)).unwrap();
    (
        store.projects().unwrap().len(),
        store.agents().unwrap().len(),
        store.events(10_000).unwrap().len(),
    )
}

#[test]
fn the_bounded_opener_never_creates_a_home() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("absent");
    let deadline = Instant::now() + Duration::from_secs(2);
    let result = Store::open_existing(&home, Duration::from_millis(100), deadline);
    assert!(
        matches!(result, Err(Error::IdentityUnavailable(_))),
        "{:?}",
        result.err()
    );
    assert!(!home.exists());

    // A directory without a database is not initialized either, and stays as it was.
    std::fs::create_dir(&home).unwrap();
    assert!(Store::open_existing(&home, Duration::from_millis(100), deadline).is_err());
    assert_eq!(std::fs::read_dir(&home).unwrap().count(), 0);
}

#[test]
fn a_bounded_store_stops_at_its_deadline() {
    let (fixture, store) = Fixture::new();
    start(&store, &fixture.shop, "one");
    drop(store);

    // An expired deadline refuses at open, with the deadline as the cause.
    let expired = Store::open_existing(&fixture.home, Duration::from_millis(100), Instant::now());
    assert!(
        matches!(expired, Err(Error::IdentityScanBudgetExceeded(ref why)) if why == "deadline"),
        "{:?}",
        expired.err()
    );

    // Every later statement on the store, reads included, stops once the deadline passes.
    let store = Store::open_existing(
        &fixture.home,
        Duration::from_millis(100),
        Instant::now() + Duration::from_millis(300),
    )
    .unwrap();
    assert_eq!(store.projects().unwrap().len(), 1);
    std::thread::sleep(Duration::from_millis(350));
    assert!(store.projects().is_err());
    assert!(store.events(10).is_err());
    let late = store.session_actor("claude", Program::Claude, &fixture.shop, None, "one", false);
    assert!(
        matches!(late, Err(Error::IdentityScanBudgetExceeded(_))),
        "{late:?}"
    );
    let late = admit(&store, &fixture.shop, "one", Vec::new());
    assert!(
        matches!(late, Err(Error::IdentityScanBudgetExceeded(_))),
        "{:?}",
        late.err()
    );
}

#[test]
fn a_pending_fence_is_settled_exactly_by_the_next_writer() {
    let (fixture, store) = Fixture::new();
    start(&store, &fixture.shop, "one");
    // A fence names the last event of its transaction: here, the binding just written.
    let agent_id = store.events(1).unwrap().remove(0).event_id;
    // A confirmed commit leaves no fence behind.
    assert!(
        !fixture.home.join(FENCE_DIR).exists()
            || std::fs::read_dir(fixture.home.join(FENCE_DIR))
                .unwrap()
                .count()
                == 0
    );

    // One operation that did commit and one that never did, both left unconfirmed.
    let committed = fixture.fence(&format!("{agent_id}.pending"));
    let rolled_back = fixture.fence("01HZZZZZZZZZZZZZZZZZZZZZZZ.pending");
    let mut settled = store.reconcile_identity_operations().unwrap();
    settled.sort_by(|a, b| a.operation.cmp(&b.operation));
    assert_eq!(settled.len(), 2);
    assert_eq!(settled[0].operation, "01HZZZZZZZZZZZZZZZZZZZZZZZ");
    assert!(!settled[0].committed);
    assert_eq!(settled[1].operation, agent_id);
    assert!(settled[1].committed);
    assert!(!committed.exists() && !rolled_back.exists());
    assert!(store.reconcile_identity_operations().unwrap().is_empty());

    // Any writer settles a pending fence before it writes: an ordinary write, a new
    // session, and an edit admission, in this or a later process.
    let fence = fixture.fence("01HYYYYYYYYYYYYYYYYYYYYYYY.pending");
    store.ensure_project("cli", &fixture.docs).unwrap();
    assert!(!fence.exists());
    let fence = fixture.fence("01HXXXXXXXXXXXXXXXXXXXXXXX.pending");
    start(&store, &fixture.shop, "two");
    assert!(!fence.exists());
    let fence = fixture.fence("01HWWWWWWWWWWWWWWWWWWWWWWW.pending");
    drop(store);
    let reopened = Store::open_quick(&fixture.home, Duration::from_millis(200)).unwrap();
    assert!(admit(&reopened, &fixture.shop, "one", Vec::new()).is_ok());
    assert!(!fence.exists());
}

#[test]
fn an_unreadable_fence_refuses_every_writer_across_restart() {
    let (fixture, store) = Fixture::new();
    start(&store, &fixture.shop, "one");
    let before = counts(&fixture.home);
    let stray = fixture.fence("not-an-operation.txt");

    let refused = |result: another_core::Result<()>| {
        assert!(
            matches!(result, Err(Error::IdentityUnavailable(ref why)) if why.contains("fence")),
            "{result:?}"
        );
    };
    refused(store.ensure_project("cli", &fixture.docs).map(|_| ()));
    refused(
        store
            .session_actor("claude", Program::Claude, &fixture.shop, None, "two", true)
            .map(|_| ()),
    );
    refused(admit(&store, &fixture.shop, "one", Vec::new()).map(|_| ()));
    refused(store.reconcile_identity_operations().map(|_| ()));
    // Reads still work, and the refusal survives a new process.
    assert_eq!(store.projects().unwrap().len(), 1);
    drop(store);
    let reopened = Store::open(&fixture.home).unwrap();
    refused(reopened.ensure_project("cli", &fixture.docs).map(|_| ()));
    assert!(stray.exists(), "an unrecognized entry is never removed");
    assert_eq!(counts(&fixture.home), before);

    // More pending operations than this build can produce also refuse.
    std::fs::remove_file(&stray).unwrap();
    for n in 0..17 {
        fixture.fence(&format!("01HV{n:022}.pending"));
    }
    refused(reopened.ensure_project("cli", &fixture.docs).map(|_| ()));
    assert_eq!(
        std::fs::read_dir(fixture.home.join(FENCE_DIR))
            .unwrap()
            .count(),
        17
    );
    assert_eq!(counts(&fixture.home), before);

    // Once an operator clears the directory, writers proceed.
    std::fs::remove_dir_all(fixture.home.join(FENCE_DIR)).unwrap();
    reopened.ensure_project("cli", &fixture.docs).unwrap();
}

#[test]
fn admission_holds_every_file_or_none() {
    let (fixture, store) = Fixture::new();
    let alice = start(&store, &fixture.shop, "alice");
    let bob = start(&store, &fixture.shop, "bob");
    store.ensure_project("cli", &fixture.docs).unwrap();

    let taken = admit(
        &store,
        &fixture.shop,
        "bob",
        vec![(fixture.docs.clone(), vec!["guide.md".into()])],
    )
    .unwrap();
    assert!(taken.admitted());
    assert_eq!(taken.agent, bob);
    assert_eq!(held_by(&store, &bob), ["docs:guide.md"]);

    // Alice's edit spans two projects; Bob holds one file in the second. Nothing of
    // Alice's is held, in either project: not the free files, not the first project.
    let stopped = admit(
        &store,
        &fixture.shop,
        "alice",
        vec![
            (
                fixture.shop.clone(),
                vec!["src/a.rs".into(), "src/b.rs".into()],
            ),
            (
                fixture.docs.clone(),
                vec!["free.md".into(), "guide.md".into()],
            ),
        ],
    )
    .unwrap();
    assert!(!stopped.admitted());
    assert_eq!(stopped.agent, alice);
    assert_eq!(stopped.conflicts.len(), 1);
    assert_eq!(stopped.conflicts[0].path, "guide.md");
    assert_eq!(stopped.conflicts[0].held.agent, bob);
    assert!(held_by(&store, &alice).is_empty());

    // Without the conflicting file, the same edit is held completely, in one step.
    let admitted = admit(
        &store,
        &fixture.shop,
        "alice",
        vec![
            (
                fixture.shop.clone(),
                vec!["src/a.rs".into(), "src/b.rs".into()],
            ),
            (fixture.docs.clone(), vec!["free.md".into()]),
        ],
    )
    .unwrap();
    assert!(admitted.admitted());
    let mut held = held_by(&store, &alice);
    held.sort();
    assert_eq!(held, ["docs:free.md", "shop:src/a.rs", "shop:src/b.rs"]);
}

#[test]
fn admission_creates_no_project_and_no_session() {
    let (fixture, store) = Fixture::new();
    let alice = start(&store, &fixture.shop, "alice");
    let before = counts(&fixture.home);

    // A project nobody registered: reported, never created, and nothing is held, not even
    // the files of the registered project in the same edit.
    let stopped = admit(
        &store,
        &fixture.shop,
        "alice",
        vec![
            (fixture.shop.clone(), vec!["src/a.rs".into()]),
            (fixture.docs.clone(), vec!["guide.md".into()]),
        ],
    )
    .unwrap();
    assert!(!stopped.admitted());
    assert_eq!(stopped.unregistered, std::slice::from_ref(&fixture.docs));
    assert!(held_by(&store, &alice).is_empty());
    assert_eq!(counts(&fixture.home), before);

    // A session that never started is not created by an edit, in a registered project or
    // an unregistered one.
    for root in [&fixture.shop, &fixture.docs] {
        let unknown = admit(
            &store,
            root,
            "never-started",
            vec![(fixture.shop.clone(), vec!["src/a.rs".into()])],
        );
        assert!(
            matches!(unknown, Err(Error::IdentityUnavailable(ref why)) if why.contains("session binding")),
            "{:?}",
            unknown.err()
        );
    }
    assert_eq!(counts(&fixture.home), before);

    // An edit larger than one admission may cover is refused whole.
    let files: Vec<String> = (0..129).map(|n| format!("src/{n}.rs")).collect();
    assert!(admit(
        &store,
        &fixture.shop,
        "alice",
        vec![(fixture.shop.clone(), files)]
    )
    .is_err());
    assert!(held_by(&store, &alice).is_empty());
    assert_eq!(counts(&fixture.home), before);
}

#[test]
fn every_admission_gives_fresh_coverage() {
    let (fixture, store) = Fixture::new();
    let alice = start(&store, &fixture.shop, "alice");
    let edit = |files: &[&str]| {
        admit(
            &store,
            &fixture.shop,
            "alice",
            vec![(
                fixture.shop.clone(),
                files.iter().map(|f| f.to_string()).collect(),
            )],
        )
        .unwrap()
    };
    assert!(edit(&["src/a.rs", "src/b.rs"]).admitted());
    let first = expiries(&fixture.home, &alice);
    assert_eq!(first.len(), 2);

    // Age both holds: one just inside the old "past half its TTL" rule, one outside it
    // with a second of life left. Neither would protect an edit that starts now.
    let now = store.now();
    let db = rusqlite::Connection::open(fixture.home.join("another.db")).unwrap();
    db.execute(
        "UPDATE holds SET expires_at = ?1 WHERE hold_id = ?2",
        rusqlite::params![now + 901_000, first[0].0],
    )
    .unwrap();
    db.execute(
        "UPDATE holds SET expires_at = ?1 WHERE hold_id = ?2",
        rusqlite::params![now + 1_000, first[1].0],
    )
    .unwrap();
    drop(db);

    // A new edit of one of the files: the same holds, both with a full TTL from now.
    assert!(edit(&["src/a.rs"]).admitted());
    let second = expiries(&fixture.home, &alice);
    assert_eq!(
        second.iter().map(|(id, _)| id).collect::<Vec<_>>(),
        first.iter().map(|(id, _)| id).collect::<Vec<_>>()
    );
    for (id, expires_at) in &second {
        assert!(
            *expires_at >= now + 1_800_000,
            "{id} expires {} ms after the admission",
            expires_at - now
        );
    }
    let renewed = store
        .events(10)
        .unwrap()
        .into_iter()
        .rev()
        .find(|e| e.kind == "hold.renewed")
        .expect("the renewal is recorded");
    assert_eq!(renewed.actor, alice);
    assert_eq!(renewed.payload["hold_ids"].as_array().unwrap().len(), 2);
}

#[test]
fn a_partly_guarded_edit_is_not_admitted() {
    let (fixture, store) = Fixture::new();
    let alice = start(&store, &fixture.shop, "alice");
    let before = counts(&fixture.home);
    let scope = |groups: Vec<(String, Vec<String>)>, unguarded: Vec<String>| {
        store.admit_edit(
            "claude",
            Program::Claude,
            &fixture.shop,
            None,
            "alice",
            "editing (test)",
            |_| EditScope { groups, unguarded },
        )
    };

    // Guarded and unguarded targets in one edit: stopped, named, nothing held or written.
    let mixed = scope(
        vec![(fixture.shop.clone(), vec!["src/a.rs".into()])],
        vec!["/scratch/b.rs".into()],
    )
    .unwrap();
    assert!(!mixed.admitted());
    assert_eq!(mixed.unguarded, ["/scratch/b.rs"]);
    assert!(held_by(&store, &alice).is_empty());
    assert_eq!(counts(&fixture.home), before);

    // Only unguarded targets: nobody guards that edit, and nothing claims to.
    let scratch = scope(Vec::new(), vec!["/scratch/b.rs".into()]).unwrap();
    assert!(scratch.admitted());
    assert!(held_by(&store, &alice).is_empty());
    assert_eq!(counts(&fixture.home), before);
}
