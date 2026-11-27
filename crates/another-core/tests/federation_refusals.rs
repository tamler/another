//! Pairing and sync refusals that still have no equivalent on main: unusable invite
//! strings and expiry boundaries, mail from a spoke the hub never allowed to send it, and
//! a spoke's hold renewals and releases reaching the hub. Pairing cases already covered
//! after #277/#280 (`pairing_rules.rs`, `pairing_attacks.rs`, `answer_signing.rs`) are
//! omitted: they now fail or duplicate those files.

use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use another_core::*;
use rusqlite::Connection;

const T0: i64 = 1_800_000_000_000;

struct Node {
    store: Store,
    db: std::path::PathBuf,
    clock: Arc<AtomicI64>,
}

impl Node {
    fn new(dir: &Path, name: &str) -> Node {
        let home = dir.join(name);
        let (clock, handle) = Clock::manual(T0);
        let test_support::Node { store, db } = test_support::Node::open(&home, name, clock);
        Node {
            store,
            db,
            clock: handle,
        }
    }

    fn sql(&self) -> Connection {
        Connection::open(&self.db).unwrap()
    }

    fn agent(&self, name: &str) -> String {
        self.store
            .register_agent(RegisterAgent {
                name: name.into(),
                program: Program::Claude,
                project: None,
                model: None,
                description: None,
            })
            .unwrap()
            .agent_id
    }

    fn id(&self) -> String {
        self.store.node_id().to_string()
    }

    fn advance_ms(&self, ms: i64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }
}

fn pair_with(hub: &Node, spoke: &Node, perms: &[Perm]) {
    test_support::pair(&hub.store, &spoke.store, perms);
}

/// Push everything `spoke` authored to the hub, as `another sync` does.
fn push(hub: &Node, spoke: &Node) -> IngestReport {
    let (events, _) = spoke.store.outbound_events(0, 10_000).unwrap();
    hub.store
        .ingest(&spoke.id(), &events, &[spoke.store.node_entry().unwrap()])
        .unwrap()
}

/// Pull everything the hub has for `spoke` into it.
fn pull(hub: &Node, spoke: &Node) -> IngestReport {
    let pulled = hub.store.pull_for(&spoke.id(), 0, 10_000).unwrap();
    spoke
        .store
        .ingest(&hub.id(), &pulled.events, &pulled.nodes)
        .unwrap()
}

/// Codes that are not invites, invites past their expiry (the expiry instant included),
/// and a hub with this node's own name are refused before anything is sent to the hub.
#[test]
fn unusable_invite_codes_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let alpha = Node::new(dir.path(), "alpha");

    let err = alpha.store.prepare_join("hello").unwrap_err();
    assert!(err.to_string().contains("not an Another invite"), "{err}");
    assert!(alpha.store.prepare_join("another-invite:%%%").is_err());
    let not_json = format!("another-invite:{}", crypto::b64(b"{\"v\":1}"));
    let err = alpha.store.prepare_join(&not_json).unwrap_err();
    assert!(err.to_string().contains("invalid invite code"), "{err}");

    let code = hub
        .store
        .create_invite("https://hub.example", &Perm::ALL, 60)
        .unwrap();
    alpha.advance_ms(59_999);
    alpha.store.prepare_join(&code).unwrap();
    alpha.advance_ms(1);
    let err = alpha.store.prepare_join(&code).unwrap_err();
    assert!(err.to_string().contains("expired"), "{err}");

    let twin = Node::new(dir.path(), "twin");
    twin.store.set_node_name("hub").unwrap();
    let code = hub
        .store
        .create_invite("https://hub.example", &Perm::ALL, 3600)
        .unwrap();
    let err = twin.store.prepare_join(&code).unwrap_err();
    assert!(err.to_string().contains("also named"), "{err}");
}

/// The hub decides what a spoke may do. A spoke without send_mail that grants itself the
/// permission locally still cannot get mail delivered on the hub.
#[test]
fn a_spoke_without_send_mail_cannot_push_mail() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let alpha = Node::new(dir.path(), "alpha");
    pair_with(&hub, &alpha, &[Perm::ReadFeed]);
    hub.agent("bob");
    let carol = alpha.agent("carol");
    assert!(pull(&hub, &alpha).rejected.is_empty());
    let mail = || {
        alpha.store.send_message(SendMessage {
            from: "carol".into(),
            to: vec!["bob@@hub".into()],
            subject: "hello".into(),
            body: "from alpha".into(),
            ..Default::default()
        })
    };
    let err = mail().unwrap_err();
    assert!(err.to_string().contains("send_mail"), "{err}");

    alpha
        .sql()
        .execute(
            "UPDATE peers SET perms = '[\"read_feed\",\"send_mail\",\"borrow_resource\"]'
             WHERE role = 'hub'",
            [],
        )
        .unwrap();
    mail().unwrap();

    let report = push(&hub, &alpha);
    assert!(
        report
            .rejected
            .iter()
            .any(|r| r.reason.contains("may not send mail")),
        "{:?}",
        report.rejected
    );
    let delivered: i64 = hub
        .sql()
        .query_row(
            "SELECT count(*) FROM messages WHERE sender_id = ?1",
            [&carol],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(delivered, 0);
}

/// A spoke's hold renewal and release reach the hub: the hub moves the expiry, then frees
/// the path, instead of keeping the hold until its original expiry.
#[test]
fn hold_renewals_and_releases_reach_the_hub() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let alpha = Node::new(dir.path(), "alpha");
    pair_with(&hub, &alpha, &Perm::ALL);
    alpha.agent("carol");
    let project_dir = dir.path().join("projects").join("shop");
    std::fs::create_dir_all(&project_dir).unwrap();
    let project = alpha
        .store
        .ensure_project("test", &project_dir.to_string_lossy())
        .unwrap();
    let hold = alpha
        .store
        .acquire_holds(AcquireHolds {
            agent: "carol".into(),
            project: project.abs_path.clone(),
            paths: vec!["src/a.rs".into()],
            ttl_seconds: Some(600),
            exclusive: true,
            reason: None,
            repo_wide: false,
        })
        .unwrap()
        .granted
        .remove(0)
        .hold_id;
    assert!(push(&hub, &alpha).rejected.is_empty());
    let on_hub = || -> (i64, Option<i64>) {
        hub.sql()
            .query_row(
                "SELECT expires_at, released_at FROM holds WHERE hold_id = ?1",
                [&hold],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    };
    assert_eq!(on_hub(), (T0 + 600_000, None));

    for node in [&hub, &alpha] {
        node.advance_ms(60_000);
    }
    alpha
        .store
        .renew_holds("carol", std::slice::from_ref(&hold))
        .unwrap();
    assert!(push(&hub, &alpha).rejected.is_empty());
    assert_eq!(on_hub(), (T0 + 60_000 + 600_000, None));

    for node in [&hub, &alpha] {
        node.advance_ms(1_000);
    }
    alpha
        .store
        .release_holds("carol", std::slice::from_ref(&hold))
        .unwrap();
    assert!(push(&hub, &alpha).rejected.is_empty());
    assert_eq!(on_hub().1, Some(T0 + 61_000));
}
