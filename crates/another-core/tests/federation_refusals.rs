//! Pairing and sync refusals that had no test: a known node presenting a new key, a hub
//! answering with another identity, a spoke joining a second hub, unusable invite codes, and
//! mail from a spoke the hub never allowed to send it. Also checks that a spoke's hold
//! renewals and releases reach its hub, so a released hold does not keep blocking there.

use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use another_core::*;
use rusqlite::{Connection, OptionalExtension};

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
        let store = Store::open_with_clock(&home, clock).unwrap();
        store.set_node_name(name).unwrap();
        Node {
            store,
            db: home.join(DB_FILE),
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

    /// The public key this node has pinned for `node`.
    fn pinned_key(&self, node: &str) -> Option<String> {
        self.sql()
            .query_row(
                "SELECT public_key FROM nodes WHERE node_id = ?1",
                [node],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
            .flatten()
    }
}

fn pair_with(hub: &Node, spoke: &Node, perms: &[Perm]) {
    let code = hub
        .store
        .create_invite("https://hub.example", perms, 3600)
        .unwrap();
    let (invite, req) = spoke.store.prepare_join(&code).unwrap();
    let resp = hub.store.accept_pairing(&req).unwrap();
    spoke.store.complete_join(&invite, &resp).unwrap();
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

fn other_key() -> String {
    crypto::public_of(&crypto::generate_key().unwrap())
}

/// Keys are pinned on first sight. A node that comes back with another key, whether it
/// pairs again or appears in a hub's node list, is refused and the pinned key stays.
#[test]
fn a_known_node_presenting_a_new_key_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let alpha = Node::new(dir.path(), "alpha");
    pair_with(&hub, &alpha, &Perm::ALL);
    let alpha_key = hub.pinned_key(&alpha.id()).unwrap();
    let hub_key = alpha.pinned_key(&hub.id()).unwrap();

    // Alpha's id pairs again with a key it never had.
    let code = hub
        .store
        .create_invite("https://hub.example", &Perm::ALL, 3600)
        .unwrap();
    let (_, mut req) = alpha.store.prepare_join(&code).unwrap();
    req.node.public_key = other_key();
    let err = hub.store.accept_pairing(&req).unwrap_err();
    assert!(err.to_string().contains("different key"), "{err}");
    assert_eq!(hub.pinned_key(&alpha.id()).unwrap(), alpha_key);

    // A hub node list that carries another key for the hub itself.
    let mut forged = hub.store.node_entry().unwrap();
    forged.public_key = other_key();
    let err = alpha.store.ingest(&hub.id(), &[], &[forged]).unwrap_err();
    assert!(err.to_string().contains("different key"), "{err}");
    assert_eq!(alpha.pinned_key(&hub.id()).unwrap(), hub_key);

    // The same entry with the pinned key is accepted.
    alpha
        .store
        .ingest(&hub.id(), &[], &[hub.store.node_entry().unwrap()])
        .unwrap();
}

/// The spoke trusts the hub named in the invite. An answer from any other identity (a
/// different node id, or the same id with another key) is refused and records no hub.
#[test]
fn a_hub_answering_with_another_identity_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let alpha = Node::new(dir.path(), "alpha");
    let code = hub
        .store
        .create_invite("https://hub.example", &Perm::ALL, 3600)
        .unwrap();
    let (invite, req) = alpha.store.prepare_join(&code).unwrap();
    let resp = hub.store.accept_pairing(&req).unwrap();

    let mut wrong_key = resp.clone();
    wrong_key.hub.public_key = other_key();
    let mut wrong_id = resp.clone();
    wrong_id.hub.node_id = "01IMPOSTORIMPOSTORIMPOSTOR".into();
    for bad in [wrong_key, wrong_id] {
        let err = alpha.store.complete_join(&invite, &bad).unwrap_err();
        assert!(err.to_string().contains("different identity"), "{err}");
        assert_eq!(alpha.store.hub().unwrap(), None);
        assert!(alpha.store.peers().unwrap().is_empty());
    }

    let peer = alpha.store.complete_join(&invite, &resp).unwrap();
    assert_eq!(peer.node_id, hub.id());
}

/// A spoke syncs with one hub. Joining a second is refused and keeps the first; joining the
/// same hub again is allowed.
#[test]
fn a_spoke_cannot_join_a_second_hub() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let other = Node::new(dir.path(), "other");
    let alpha = Node::new(dir.path(), "alpha");
    pair_with(&hub, &alpha, &Perm::ALL);

    let code = other
        .store
        .create_invite("https://other.example", &Perm::ALL, 3600)
        .unwrap();
    let (invite, req) = alpha.store.prepare_join(&code).unwrap();
    let resp = other.store.accept_pairing(&req).unwrap();
    let err = alpha.store.complete_join(&invite, &resp).unwrap_err();
    assert!(err.to_string().contains("another hub"), "{err}");
    assert_eq!(
        alpha.store.hub().unwrap(),
        Some((hub.id(), "https://hub.example".to_string()))
    );
    assert_eq!(
        alpha.pinned_key(&other.id()),
        None,
        "the refused hub was recorded"
    );

    pair_with(&hub, &alpha, &[Perm::ReadFeed]);
    assert_eq!(alpha.store.hub().unwrap().unwrap().0, hub.id());
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
