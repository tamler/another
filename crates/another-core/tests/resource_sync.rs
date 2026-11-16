//! Leases on another node's resource: a borrower's renewal must reach the node that owns
//! the resource, or that node lets the lease lapse while it is still in use; a resource
//! whose owner is gone cannot be borrowed from afar. Also the local refusals around
//! renewal: a lease the caller does not have, and a non-positive TTL.

use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use another_core::*;
use rusqlite::Connection;
use serde_json::Value;

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

    fn agent(&self, name: &str) {
        self.store
            .register_agent(RegisterAgent {
                name: name.into(),
                program: Program::Claude,
                project: None,
                model: None,
                description: None,
            })
            .unwrap();
    }

    fn id(&self) -> String {
        self.store.node_id().to_string()
    }

    fn lease_expiry(&self, lease_id: &str) -> i64 {
        self.sql()
            .query_row(
                "SELECT expires_at FROM resource_leases WHERE lease_id = ?1",
                [lease_id],
                |r| r.get(0),
            )
            .unwrap()
    }
}

/// A hub and a spoke paired with every permission, both at T0.
fn fed(dir: &Path) -> (Node, Node) {
    let hub = Node::new(dir, "hub");
    let alpha = Node::new(dir, "alpha");
    let code = hub
        .store
        .create_invite("https://hub.example", &Perm::ALL, 3600)
        .unwrap();
    let (invite, req) = alpha.store.prepare_join(&code).unwrap();
    let resp = hub.store.accept_pairing(&req).unwrap();
    alpha.store.complete_join(&invite, &resp).unwrap();
    (hub, alpha)
}

fn push(hub: &Node, spoke: &Node) -> IngestReport {
    let (events, _) = spoke.store.outbound_events(0, 10_000).unwrap();
    hub.store
        .ingest(&spoke.id(), &events, &[spoke.store.node_entry().unwrap()])
        .unwrap()
}

fn pull(hub: &Node, spoke: &Node) -> IngestReport {
    let pulled = hub.store.pull_for(&spoke.id(), 0, 10_000).unwrap();
    spoke
        .store
        .ingest(&hub.id(), &pulled.events, &pulled.nodes)
        .unwrap()
}

/// A shared resource registered by `agent`, reachable on `port`.
fn register(node: &Node, agent: &str, port: u16) -> String {
    node.store
        .register_resource(RegisterResource {
            agent: agent.into(),
            kind: ResourceKind::ModelServer,
            name: format!("model@{port}"),
            endpoint: Some(format!("http://127.0.0.1:{port}")),
            port: Some(port),
            pid: None,
            project: None,
            purpose: None,
            mode: LeaseMode::Shared,
            ttl_seconds: Some(600),
            metadata: None,
        })
        .unwrap()
        .resource
        .resource_id
}

/// Carol on alpha borrows the hub's resource and later renews it with a heartbeat. The
/// renewal names the hub as the resource's node and the hub moves the lease's expiry.
#[test]
fn a_borrowers_renewal_reaches_the_node_that_owns_the_resource() {
    let dir = tempfile::tempdir().unwrap();
    let (hub, alpha) = fed(dir.path());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    hub.agent("alice");
    alpha.agent("carol");
    let rid = register(&hub, "alice", port);
    assert!(pull(&hub, &alpha).rejected.is_empty());

    let lease = alpha
        .store
        .borrow_resource("carol", &ResourceRef::Id(rid.clone()), Some(900))
        .unwrap()
        .lease
        .unwrap()
        .lease_id;
    assert!(push(&hub, &alpha).rejected.is_empty());
    // The lease's own TTL (900 s), not the resource owner's (600 s).
    assert_eq!(hub.lease_expiry(&lease), T0 + 900_000);

    for node in [&hub, &alpha] {
        node.clock.fetch_add(60_000, Ordering::SeqCst);
    }
    alpha.store.heartbeat_resources("carol", &[]).unwrap();
    let renewed: String = alpha
        .sql()
        .query_row(
            "SELECT payload FROM events WHERE kind = 'resource.renewed' ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let renewed: Value = serde_json::from_str(&renewed).unwrap();
    assert_eq!(renewed["resource_nodes"], serde_json::json!([hub.id()]));
    assert_eq!(renewed["lease_ids"], serde_json::json!([lease]));

    assert!(push(&hub, &alpha).rejected.is_empty());
    assert_eq!(hub.lease_expiry(&lease), T0 + 60_000 + 900_000);
    drop(listener);
}

/// The owner is a spoke that does not read the feed. The hub relays the renewal to it only
/// because the renewal names it, so the owner's copy of the borrower's lease is extended. A
/// renewal that does not name it (of a lease on alpha's own resource) does not reach it.
#[test]
fn a_renewal_reaches_an_owner_spoke_that_does_not_read_the_feed() {
    let dir = tempfile::tempdir().unwrap();
    let (hub, alpha) = fed(dir.path());
    let beta = Node::new(dir.path(), "beta");
    let code = hub
        .store
        .create_invite("https://hub.example", &[Perm::BorrowResource], 3600)
        .unwrap();
    let (invite, req) = beta.store.prepare_join(&code).unwrap();
    let resp = hub.store.accept_pairing(&req).unwrap();
    beta.store.complete_join(&invite, &resp).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    beta.agent("dave");
    alpha.agent("carol");
    let rid = register(&beta, "dave", port);
    assert!(push(&hub, &beta).rejected.is_empty());
    assert!(push(&hub, &alpha).rejected.is_empty());
    assert!(pull(&hub, &alpha).rejected.is_empty());
    assert!(pull(&hub, &beta).rejected.is_empty());

    let lease = alpha
        .store
        .borrow_resource("carol", &ResourceRef::Id(rid), Some(600))
        .unwrap()
        .lease
        .unwrap()
        .lease_id;
    assert!(push(&hub, &alpha).rejected.is_empty());
    assert!(pull(&hub, &beta).rejected.is_empty());
    assert_eq!(beta.lease_expiry(&lease), T0 + 600_000);

    for node in [&hub, &alpha, &beta] {
        node.clock.fetch_add(60_000, Ordering::SeqCst);
    }
    alpha.store.heartbeat_resources("carol", &[]).unwrap();
    assert!(push(&hub, &alpha).rejected.is_empty());
    assert!(pull(&hub, &beta).rejected.is_empty());
    assert_eq!(beta.lease_expiry(&lease), T0 + 60_000 + 600_000);

    let own = register(&alpha, "carol", 9);
    alpha
        .store
        .heartbeat_resources("carol", &[ResourceRef::Id(own)])
        .unwrap();
    let unnamed: String = alpha
        .sql()
        .query_row(
            "SELECT event_id FROM events WHERE kind = 'resource.renewed' ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(push(&hub, &alpha).rejected.is_empty());
    let on_hub: i64 = hub
        .sql()
        .query_row(
            "SELECT count(*) FROM events WHERE event_id = ?1",
            [&unnamed],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(on_hub, 1, "the hub has the renewal to relay");
    assert!(pull(&hub, &beta).rejected.is_empty());
    let on_beta: i64 = beta
        .sql()
        .query_row(
            "SELECT count(*) FROM events WHERE event_id = ?1",
            [&unnamed],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(on_beta, 0, "a renewal that does not name beta reached it");
    drop(listener);
}

/// Once the hub's owner lets go, alpha sees the resource without a live owner and cannot
/// borrow it: only the hub can adopt it.
#[test]
fn a_foreign_resource_without_a_live_owner_cannot_be_borrowed() {
    let dir = tempfile::tempdir().unwrap();
    let (hub, alpha) = fed(dir.path());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    hub.agent("alice");
    alpha.agent("carol");
    let rid = register(&hub, "alice", port);
    hub.store
        .release_resource("alice", &ResourceRef::Id(rid.clone()), false)
        .unwrap();
    assert!(pull(&hub, &alpha).rejected.is_empty());

    let err = alpha
        .store
        .borrow_resource("carol", &ResourceRef::Id(rid.clone()), Some(600))
        .unwrap_err();
    assert!(err.to_string().contains("no live owner"), "{err}");
    let leases: i64 = alpha
        .sql()
        .query_row(
            "SELECT count(*) FROM resource_leases WHERE resource_id = ?1 AND released_at IS NULL",
            [&rid],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(leases, 0);
    drop(listener);
}

/// Renewing a named resource the caller holds no lease on is an error, not a silent no-op,
/// and renews nothing else; a TTL of zero or less is refused before anything is written.
#[test]
fn renewal_and_ttl_refusals() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::new(dir.path(), "solo");
    node.agent("alice");
    node.agent("bob");
    let rid = register(&node, "alice", 9);
    let owner_lease: String = node
        .sql()
        .query_row(
            "SELECT lease_id FROM resource_leases WHERE resource_id = ?1",
            [&rid],
            |r| r.get(0),
        )
        .unwrap();
    let other = register(&node, "bob", 10);

    node.clock.fetch_add(60_000, Ordering::SeqCst);
    let err = node
        .store
        .heartbeat_resources(
            "bob",
            &[ResourceRef::Id(other.clone()), ResourceRef::Id(rid.clone())],
        )
        .unwrap_err();
    assert!(err.to_string().contains("no live lease"), "{err}");
    let bob_lease: String = node
        .sql()
        .query_row(
            "SELECT lease_id FROM resource_leases WHERE resource_id = ?1",
            [&other],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        node.lease_expiry(&bob_lease),
        T0 + 600_000,
        "bob's lease was renewed"
    );
    assert_eq!(node.lease_expiry(&owner_lease), T0 + 600_000);

    for ttl in [0, -5] {
        let err = node
            .store
            .borrow_resource("bob", &ResourceRef::Id(rid.clone()), Some(ttl))
            .unwrap_err();
        assert!(err.to_string().contains("must be positive"), "{err}");
    }
    assert_eq!(
        node.store.resource(&ResourceRef::Id(rid)).unwrap().refcount,
        1
    );
}
