//! Project notes (D70): short facts any local agent leaves for a project, shown to every
//! agent working in it or a sibling checkout. Notes are node-local, their text never
//! enters an event, the feed or a peer, and forgetting one really removes the text.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use another_core::*;
use sha2::{Digest, Sha256};

const T0: i64 = 1_800_000_000_000;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// `shop` and its worktree `shop-wt` are one repository; `blog` is another.
struct Fixture {
    _dir: tempfile::TempDir,
    home: PathBuf,
    store: Store,
    clock: Arc<AtomicI64>,
    shop: PathBuf,
    shop_wt: PathBuf,
    blog: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let shop = root.join("shop");
        write(
            &shop.join(".git/config"),
            "[remote \"origin\"]\n\turl = git@example.com:me/shop.git\n",
        );
        let shop_wt = root.join("shop-wt");
        let wt_git = shop.join(".git/worktrees/shop-wt");
        write(
            &shop_wt.join(".git"),
            &format!("gitdir: {}\n", wt_git.display()),
        );
        write(&wt_git.join("commondir"), "../..\n");
        let blog = root.join("blog");
        write(
            &blog.join(".git/config"),
            "[remote \"origin\"]\n\turl = https://example.com/me/blog\n",
        );
        let (clock, handle) = Clock::manual(T0);
        // On disk, so tests can read the notes table itself.
        let home = root.join("home");
        let store = Store::open_with_clock(&home, clock).unwrap();
        for (name, project) in [("alice", &shop), ("bob", &shop_wt), ("carol", &blog)] {
            store
                .register_agent(RegisterAgent {
                    name: name.into(),
                    program: Program::Claude,
                    project: Some(project.to_string_lossy().into_owned()),
                    model: None,
                    description: None,
                })
                .unwrap();
        }
        Fixture {
            _dir: dir,
            home,
            store,
            clock: handle,
            shop,
            shop_wt,
            blog,
        }
    }

    fn path(&self, p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    fn set(&self, agent: &str, project: &Path, key: Option<&str>, text: &str) -> Result<Note> {
        self.store.set_note(NewNote {
            agent: agent.into(),
            project: self.path(project),
            key: key.map(str::to_owned),
            text: text.into(),
            ttl_seconds: None,
        })
    }

    fn texts(&self, project: &Path) -> Vec<String> {
        self.store
            .notes(&self.path(project), 50)
            .unwrap()
            .notes
            .into_iter()
            .map(|n| n.text)
            .collect()
    }

    /// Every `text` stored in the notes table, live or not.
    fn stored_texts(&self) -> Vec<String> {
        let db = rusqlite::Connection::open(self.home.join("another.db")).unwrap();
        let mut stmt = db.prepare("SELECT text FROM notes").unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// Every event payload, as stored.
    fn payloads(&self) -> Vec<String> {
        self.store
            .events(10_000)
            .unwrap()
            .into_iter()
            .map(|e| e.payload.to_string())
            .collect()
    }
}

fn sha256(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

#[test]
fn a_note_reaches_its_project_and_sibling_checkouts_only() {
    let fx = Fixture::new();
    let note = fx
        .set(
            "alice",
            &fx.shop,
            Some("e2e"),
            "e2e tests need the dev server on :3000",
        )
        .unwrap();
    assert_eq!(note.author, "alice@shop");
    assert_eq!(note.key.as_deref(), Some("e2e"));
    assert_eq!(
        fx.texts(&fx.shop),
        ["e2e tests need the dev server on :3000"]
    );
    assert_eq!(
        fx.texts(&fx.shop_wt),
        ["e2e tests need the dev server on :3000"],
        "a sibling worktree of the same repository sees the note"
    );
    assert!(fx.texts(&fx.blog).is_empty(), "another project does not");
}

/// Everything a list must leave alone: cached repository links, events and notes.
fn db_state(fx: &Fixture) -> Vec<String> {
    let db = rusqlite::Connection::open(fx.home.join("another.db")).unwrap();
    let mut out = Vec::new();
    for sql in [
        "SELECT project_id || '|' || repo_key || '|' || repo_prefix || '|' || checked_at
         FROM project_repos ORDER BY project_id",
        "SELECT 'events ' || count(*) FROM events",
        "SELECT note_id || '|' || text || '|' || ifnull(deleted_at, '') FROM notes ORDER BY note_id",
        "SELECT 'projects ' || count(*) FROM projects",
    ] {
        let mut stmt = db.prepare(sql).unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        out.extend(rows.map(Result::unwrap));
    }
    out
}

fn cache_sql(fx: &Fixture, project: &Path, sql: &str) {
    // By slug: the stored path is the canonical one, which differs from the fixture's on
    // some platforms (Windows).
    let slug = fx.store.notes(&fx.path(project), 1).unwrap().project;
    let db = rusqlite::Connection::open(fx.home.join("another.db")).unwrap();
    let changed = db
        .execute(
            &format!("{sql} WHERE project_id = (SELECT project_id FROM projects WHERE slug = ?1)"),
            [slug],
        )
        .unwrap();
    assert_eq!(
        changed, 1,
        "the fixture project has a cached link to change"
    );
}

#[test]
fn a_missing_or_outdated_cached_link_does_not_hide_a_sibling() {
    let fx = Fixture::new();
    fx.set("alice", &fx.shop, None, "fact from shop").unwrap();
    fx.clock.fetch_add(1000, Ordering::SeqCst);
    fx.set("bob", &fx.shop_wt, None, "fact from the worktree")
        .unwrap();
    let both = ["fact from the worktree", "fact from shop"];

    // A project whose link was never computed (registered before links existed).
    cache_sql(&fx, &fx.shop_wt, "DELETE FROM project_repos");
    let before = db_state(&fx);
    assert_eq!(fx.texts(&fx.shop_wt), both, "listing the uncached sibling");
    assert_eq!(
        fx.texts(&fx.shop),
        both,
        "listing beside an uncached sibling"
    );
    assert_eq!(db_state(&fx), before, "a list writes nothing");

    // A link cached under older key rules is not trusted either.
    cache_sql(
        &fx,
        &fx.shop,
        "UPDATE project_repos SET repo_key = 'v1:something-else'",
    );
    let before = db_state(&fx);
    assert_eq!(fx.texts(&fx.shop_wt), both);
    assert_eq!(fx.texts(&fx.shop), both);
    assert_eq!(db_state(&fx), before, "a list writes nothing");
    assert!(fx.texts(&fx.blog).is_empty());
}

#[test]
fn a_stale_cached_link_does_not_show_another_repositorys_notes() {
    let fx = Fixture::new();
    fx.set("alice", &fx.shop, None, "fact from shop").unwrap();
    fx.set("bob", &fx.shop_wt, None, "fact from the old worktree")
        .unwrap();
    // The worktree directory now holds a different repository; its cache still says shop.
    std::fs::remove_file(fx.shop_wt.join(".git")).unwrap();
    write(
        &fx.shop_wt.join(".git/config"),
        "[remote \"origin\"]\n\turl = https://example.com/elsewhere/blog.git\n",
    );
    let before = db_state(&fx);
    assert_eq!(
        fx.texts(&fx.shop_wt),
        ["fact from the old worktree"],
        "the directory sees only its own notes, not shop's"
    );
    assert_eq!(
        fx.texts(&fx.shop),
        ["fact from shop"],
        "shop no longer sees notes of a directory that left the repository"
    );
    assert_eq!(db_state(&fx), before, "a list writes nothing");
}

#[test]
fn a_different_path_in_the_same_repository_keeps_its_own_notes() {
    let fx = Fixture::new();
    let pkg = fx.shop.join("pkg");
    let wt_pkg = fx.shop_wt.join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::create_dir_all(&wt_pkg).unwrap();
    for (name, project) in [("dave", &pkg), ("erin", &wt_pkg)] {
        fx.store
            .register_agent(RegisterAgent {
                name: name.into(),
                program: Program::Claude,
                project: Some(fx.path(project)),
                model: None,
                description: None,
            })
            .unwrap();
    }
    fx.set("alice", &fx.shop, None, "fact from the top level")
        .unwrap();
    fx.set("dave", &pkg, None, "fact from pkg").unwrap();
    // Without any cached links, the computed prefixes still keep the packages apart.
    for project in [&fx.shop, &fx.shop_wt, &pkg, &wt_pkg] {
        cache_sql(&fx, project, "DELETE FROM project_repos");
    }
    assert_eq!(fx.texts(&fx.shop), ["fact from the top level"]);
    assert_eq!(fx.texts(&fx.shop_wt), ["fact from the top level"]);
    assert_eq!(fx.texts(&pkg), ["fact from pkg"]);
    assert_eq!(
        fx.texts(&wt_pkg),
        ["fact from pkg"],
        "the same path in a sibling checkout shares the notes"
    );
}

#[test]
fn notes_list_newest_first_and_a_key_is_replaced_in_place() {
    let fx = Fixture::new();
    fx.set("alice", &fx.shop, Some("db"), "use the shared postgres")
        .unwrap();
    fx.clock.fetch_add(1000, Ordering::SeqCst);
    fx.set("alice", &fx.shop, None, "schema.rs is generated")
        .unwrap();
    fx.clock.fetch_add(1000, Ordering::SeqCst);
    let replaced = fx
        .set("bob", &fx.shop, Some("db"), "postgres moved to :5433")
        .unwrap();
    assert_eq!(
        replaced.author, "bob@shop-wt",
        "the last writer is the author"
    );
    assert_eq!(
        fx.texts(&fx.shop),
        ["postgres moved to :5433", "schema.rs is generated"]
    );
    // The replaced text is gone, not hidden.
    assert!(!fx
        .stored_texts()
        .iter()
        .any(|t| t.contains("shared postgres")));
    assert!(!fx.payloads().iter().any(|p| p.contains("shared postgres")));
    let list = fx.store.notes(&fx.path(&fx.shop), 50).unwrap();
    assert_eq!(list.total, 2);
}

#[test]
fn forgetting_a_note_removes_its_text_everywhere() {
    let fx = Fixture::new();
    let secret = "the staging password is hunter2-do-not-share";
    let note = fx.set("alice", &fx.shop, None, secret).unwrap();
    fx.set("alice", &fx.shop, Some("keep"), "keep this one")
        .unwrap();
    let gone = fx
        .store
        .forget_note("bob", NoteTarget::Id(note.note_id.clone()))
        .unwrap();
    assert_eq!(gone.note_id, note.note_id);
    assert_eq!(gone.bytes, secret.len());
    assert_eq!(fx.texts(&fx.shop), ["keep this one"]);

    // The text is gone from the notes table and from every event payload, and its hash
    // appears only in the event that set it.
    assert!(!fx.stored_texts().iter().any(|t| t.contains("hunter2")));
    let payloads = fx.payloads();
    assert!(
        !payloads.iter().any(|p| p.contains("hunter2")),
        "{payloads:?}"
    );
    let hash = sha256(secret);
    let with_hash: Vec<_> = payloads.iter().filter(|p| p.contains(&hash)).collect();
    assert_eq!(with_hash.len(), 1, "{with_hash:?}");

    // Forgetting by key works too, and an unknown note is a clear error.
    fx.store
        .forget_note(
            "alice",
            NoteTarget::Key {
                project: fx.path(&fx.shop_wt),
                key: "keep".into(),
            },
        )
        .unwrap_err();
    fx.store
        .forget_note(
            "alice",
            NoteTarget::Key {
                project: fx.path(&fx.shop),
                key: "keep".into(),
            },
        )
        .unwrap();
    assert!(fx.texts(&fx.shop).is_empty());
    assert!(matches!(
        fx.store.forget_note("alice", NoteTarget::Id(note.note_id)),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn note_text_never_reaches_peers_or_the_feed() {
    let fx = Fixture::new();
    let text = "flaky test is checkout_spec, ignore it";
    let note = fx.set("alice", &fx.shop, Some("flaky"), text).unwrap();
    fx.store
        .forget_note("alice", NoteTarget::Id(note.note_id))
        .unwrap();
    let (outbound, _) = fx.store.outbound_events(0, 10_000).unwrap();
    assert!(
        !outbound.iter().any(|e| e.kind.starts_with("note.")),
        "note events never leave the node"
    );
    let feed = fx
        .store
        .activity(ActivityQuery {
            since: None,
            limit: 500,
            kinds: Vec::new(),
        })
        .unwrap();
    let notes: Vec<_> = feed
        .items
        .iter()
        .filter(|i| i.kind.starts_with("note."))
        .collect();
    assert_eq!(notes.len(), 2, "set and forgotten are in the local feed");
    let rendered = serde_json::to_string(&feed).unwrap();
    assert!(!rendered.contains("checkout_spec"), "{rendered}");
    assert!(!rendered.contains(&sha256(text)), "{rendered}");
    assert!(notes[0].summary.contains("flaky") || notes[1].summary.contains("flaky"));
}

#[test]
fn caps_refuse_oversized_notes_and_a_201st_live_note_with_a_hint() {
    let fx = Fixture::new();
    let too_big = "x".repeat(MAX_NOTE_BYTES + 1);
    let err = fx.set("alice", &fx.shop, None, &too_big).unwrap_err();
    assert!(err.to_string().contains("4096"), "{err}");
    fx.set("alice", &fx.shop, None, &"x".repeat(MAX_NOTE_BYTES))
        .unwrap();
    for n in 1..MAX_LIVE_NOTES {
        fx.set("alice", &fx.shop, Some(&format!("k{n}")), "fact")
            .unwrap();
    }
    let err = fx.set("alice", &fx.shop, None, "one more").unwrap_err();
    assert!(err.to_string().contains("forget"), "{err}");
    // Replacing a key is not a new note, and other projects have their own budget.
    fx.set("alice", &fx.shop, Some("k1"), "updated fact")
        .unwrap();
    fx.set("carol", &fx.blog, None, "blog fact").unwrap();
}

#[test]
fn invalid_notes_are_refused() {
    let fx = Fixture::new();
    assert!(
        fx.set("alice", &fx.shop, None, "   ").is_err(),
        "blank text"
    );
    assert!(
        fx.set("alice", &fx.shop, Some(""), "x").is_err(),
        "empty key"
    );
    assert!(
        fx.set("alice", &fx.shop, Some("a\nb"), "x").is_err(),
        "control characters in a key"
    );
    assert!(
        fx.set("nobody", &fx.shop, None, "x").is_err(),
        "unknown agent"
    );
    assert!(
        fx.set("alice", Path::new("/nowhere/unregistered"), None, "x")
            .is_err(),
        "unregistered project"
    );
    assert!(fx
        .store
        .set_note(NewNote {
            agent: "alice".into(),
            project: fx.path(&fx.shop),
            key: None,
            text: "x".into(),
            ttl_seconds: Some(0),
        })
        .is_err());
}

#[test]
fn an_expired_note_is_hidden_then_erased_by_the_sweep() {
    let fx = Fixture::new();
    fx.store
        .set_note(NewNote {
            agent: "alice".into(),
            project: fx.path(&fx.shop),
            key: Some("server".into()),
            text: "dev server is already on :3000".into(),
            ttl_seconds: Some(600),
        })
        .unwrap();
    assert_eq!(fx.texts(&fx.shop).len(), 1);
    fx.clock.fetch_add(601_000, Ordering::SeqCst);
    assert!(
        fx.texts(&fx.shop).is_empty(),
        "expired notes are not listed"
    );
    // The sweep erases the expired text.
    fx.store.sweep().unwrap();
    assert!(!fx.stored_texts().iter().any(|t| t.contains(":3000")));
    assert!(fx
        .store
        .events(10_000)
        .unwrap()
        .iter()
        .any(|e| e.kind == "note.expired"));
}

#[test]
fn an_expired_note_does_not_block_its_key_before_the_sweep() {
    let fx = Fixture::new();
    fx.store
        .set_note(NewNote {
            agent: "alice".into(),
            project: fx.path(&fx.shop),
            key: Some("server".into()),
            text: "dev server is already on :3000".into(),
            ttl_seconds: Some(600),
        })
        .unwrap();
    fx.clock.fetch_add(601_000, Ordering::SeqCst);
    fx.set("alice", &fx.shop, Some("server"), "dev server is on :4000")
        .unwrap();
    assert_eq!(fx.texts(&fx.shop), ["dev server is on :4000"]);
    assert!(!fx.stored_texts().iter().any(|t| t.contains(":3000")));
}
