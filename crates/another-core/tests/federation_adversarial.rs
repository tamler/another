//! Adversarial pass on the federation authority fix (#141): revoked peers, clock skew and
//! unbounded TTLs, node-name impersonation, and state stored before the fix. Every attack
//! is a correctly signed event from a paired spoke ("mallory").
//!
//! Tests named `gap_*` fail at #141's head `f236faf` and describe the expected behaviour.
//! Tests named `control_*` pass and pin behaviour that should stay.

use std::path::Path;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

use another_core::test_support::forge_event;
use another_core::*;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};

const T0: i64 = 1_800_000_000_000;
const DAY_MS: i64 = 86_400_000;

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
}

fn pair(hub: &Node, spoke: &Node) {
    let code = hub
        .store
        .create_invite("https://hub.example", &Perm::ALL, 3600)
        .unwrap();
    let (invite, req) = spoke.store.prepare_join(&code).unwrap();
    let resp = hub.store.accept_pairing(&req).unwrap();
    spoke.store.complete_join(&invite, &resp).unwrap();
}

/// Push everything `spoke` authored to the hub, as `another sync` does.
fn push(hub: &Node, spoke: &Node) -> IngestReport {
    let (events, _) = spoke.store.outbound_events(0, 10_000).unwrap();
    hub.store.ingest(&spoke.id(), &events, &[]).unwrap()
}

struct Fed {
    _dir: tempfile::TempDir,
    hub: Node,
    alpha: Node,
    mallory: Node,
    seq: AtomicU64,
}

impl Fed {
    fn new() -> Fed {
        let dir = tempfile::tempdir().unwrap();
        let hub = Node::new(dir.path(), "hub");
        let alpha = Node::new(dir.path(), "alpha");
        let mallory = Node::new(dir.path(), "mallory");
        pair(&hub, &alpha);
        pair(&hub, &mallory);
        Fed {
            _dir: dir,
            hub,
            alpha,
            mallory,
            seq: AtomicU64::new(0),
        }
    }

    /// A correctly signed mallory event with origin time `ts`, pushed to the hub.
    fn forge_at(&self, ts: i64, kind: &str, payload: Value) -> IngestReport {
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let ev = forge_event(
            &self.mallory.sql(),
            WireEvent {
                seq: 2_000_000 + n as i64,
                event_id: format!("adv-{kind}-{n}"),
                node_id: self.mallory.id(),
                ts,
                actor: "mallory".into(),
                kind: kind.into(),
                payload: payload.to_string(),
                prev_id: None,
                sig: String::new(),
            },
        );
        self.hub
            .store
            .ingest(&self.mallory.id(), &[ev], &[])
            .unwrap()
    }

    fn forge(&self, kind: &str, payload: Value) -> IngestReport {
        self.forge_at(T0 + 1_000, kind, payload)
    }

    /// A shared resource owned by hub agent `alice`, with no other leases.
    fn hub_resource(&self) -> String {
        self.hub.agent("alice");
        self.hub
            .store
            .register_resource(RegisterResource {
                agent: "alice".into(),
                kind: ResourceKind::ModelServer,
                name: "ollama@11434".into(),
                endpoint: Some("http://127.0.0.1:11434".into()),
                port: None,
                pid: None,
                project: None,
                purpose: None,
                mode: LeaseMode::Shared,
                ttl_seconds: None,
                metadata: None,
            })
            .unwrap()
            .resource
            .resource_id
    }

    /// Mallory's agent `spy`, known to the hub.
    fn spy(&self) -> String {
        let spy = self.mallory.agent("spy");
        assert!(push(&self.hub, &self.mallory).rejected.is_empty());
        spy
    }

    fn owner_node(&self, resource: &str) -> Option<String> {
        self.hub
            .sql()
            .query_row(
                "SELECT l.node_id FROM resource_leases l
                 WHERE l.resource_id = ?1 AND l.role = 'owner' AND l.released_at IS NULL",
                [resource],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
    }
}

/// P1. Revoking a spoke does not end its leases on the hub's resources. When the hub's owner
/// leaves, `settle` promotes the revoked node's borrower lease to owner and announces the
/// transfer to every other spoke.
#[test]
fn gap_a_revoked_spoke_does_not_inherit_a_hub_resource() {
    let fed = Fed::new();
    let resource = fed.hub_resource();
    let spy = fed.spy();
    let borrowed = fed.forge(
        "resource.borrowed",
        json!({
            "resource_id": resource, "lease_id": "spy-lease", "agent_id": spy,
            "ttl_seconds": 1800, "resource_node": fed.hub.id(),
        }),
    );
    assert_eq!(borrowed.accepted, 1, "an authorized borrow: {borrowed:?}");

    fed.hub.store.revoke_peer("mallory").unwrap();
    // The hub's owner leaves while the revoked node's lease is still inside its TTL.
    fed.hub
        .store
        .release_resource("alice", &ResourceRef::Id(resource.clone()), false)
        .unwrap();

    assert_ne!(
        fed.owner_node(&resource).as_deref(),
        Some(fed.mallory.id().as_str()),
        "a revoked node became the owner of the hub's resource"
    );
}

/// P1. The lease expiry of a foreign borrow is the origin's clock plus the origin's TTL,
/// unbounded. A spoke with its clock ten years ahead (or a TTL of ten years) holds a lease
/// on the hub's resource that the hub never expires: only the origin's own
/// `resource.lease_expired` ends it. After the owner leaves, that lease becomes the owner.
#[test]
fn gap_a_skewed_foreign_lease_does_not_outlive_the_maximum_ttl() {
    let fed = Fed::new();
    let resource = fed.hub_resource();
    let spy = fed.spy();
    let ten_years = 10 * 365 * DAY_MS;
    let report = fed.forge_at(
        T0 + ten_years,
        "resource.borrowed",
        json!({
            "resource_id": resource, "lease_id": "skewed-lease", "agent_id": spy,
            "ttl_seconds": 1800, "resource_node": fed.hub.id(),
        }),
    );
    let huge = fed.forge(
        "resource.borrowed",
        json!({
            "resource_id": resource, "lease_id": "huge-ttl-lease", "agent_id": spy,
            "ttl_seconds": 315_360_000, "resource_node": fed.hub.id(),
        }),
    );
    // Either refusing the events or bounding the lease is acceptable; what must not happen
    // is a lease that the hub sees as live a week later.
    fed.hub.advance_ms(7 * DAY_MS);
    let now = T0 + 7 * DAY_MS;
    let live: i64 = fed
        .hub
        .sql()
        .query_row(
            "SELECT count(*) FROM resource_leases
             WHERE resource_id = ?1 AND node_id = ?2 AND released_at IS NULL AND expires_at > ?3",
            rusqlite::params![resource, fed.mallory.id(), now],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        live, 0,
        "foreign leases still live a week later on the hub: skewed {report:?}, huge {huge:?}"
    );
}

/// P2. Pairing refuses a duplicate node name, but a later `node.renamed` from a spoke is
/// projected without that check. Mallory renames itself "alpha"; its agent then shows the
/// same `name@@node` address as alpha's agent, and an address lookup by node name picks
/// one of the two.
#[test]
fn gap_a_spoke_cannot_rename_itself_to_another_nodes_name() {
    let fed = Fed::new();
    let alpha_bob = fed.alpha.agent("bob");
    assert!(push(&fed.hub, &fed.alpha).rejected.is_empty());
    // Mallory's own history first, so its genuine "mallory" name event is already applied.
    assert!(push(&fed.hub, &fed.mallory).rejected.is_empty());

    let renamed = fed.forge(
        "node.renamed",
        json!({ "node_id": fed.mallory.id(), "display_name": "alpha" }),
    );
    let bob = fed.mallory.agent("bob");
    assert!(push(&fed.hub, &fed.mallory).rejected.is_empty());

    // Mail from the hub to alpha's bob, by address.
    fed.hub.agent("carol");
    let sent = fed.hub.store.send_message(SendMessage {
        from: "carol".into(),
        to: vec!["bob@@alpha".into()],
        subject: "for alpha's bob".into(),
        body: "private".into(),
        ..Default::default()
    });
    let delivered_to: Option<String> = sent.ok().and_then(|m| {
        fed.hub
            .sql()
            .query_row(
                "SELECT agent_id FROM deliveries WHERE message_id = ?1 AND role = 'to'",
                [&m.message_id],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
    });
    eprintln!(
        "bob@@alpha resolved to {} on the hub",
        match delivered_to.as_deref() {
            Some(id) if id == alpha_bob => "alpha's bob",
            Some(id) if id == bob => "MALLORY's bob",
            other =>
                if other.is_none() {
                    "nothing"
                } else {
                    "someone else"
                },
        }
    );

    let names: i64 = fed
        .hub
        .sql()
        .query_row(
            "SELECT count(*) FROM nodes WHERE display_name = 'alpha'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let address: Option<String> = fed
        .hub
        .sql()
        .query_row(
            "SELECT address FROM agents WHERE agent_id = ?1",
            [&bob],
            |r| r.get(0),
        )
        .optional()
        .unwrap();
    assert_eq!(
        names, 1,
        "two nodes are named alpha on the hub (rename {renamed:?}); mallory's bob reads as {address:?}"
    );
}

/// P3 control. An event a pre-fix hub already stored (here, mallory forging alpha's
/// session end) is still in the hub's log and is relayed to a spoke that joins later.
/// The new spoke rejects it and keeps syncing, but the hub's own projection was changed
/// before the upgrade and nothing repairs it.
#[test]
fn control_a_forged_event_stored_before_the_fix_is_rejected_downstream() {
    let fed = Fed::new();
    let bob = fed.alpha.agent("bob");
    assert!(push(&fed.hub, &fed.alpha).rejected.is_empty());

    // Simulate a pre-#141 hub: a correctly signed forged event written straight into its
    // log and projection, as the unfixed ingest would have done.
    let secret: String = fed
        .mallory
        .sql()
        .query_row("SELECT secret_key FROM nodes WHERE is_local = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    let key = crypto::decode_secret(&secret).unwrap();
    let payload = json!({ "agent_id": bob }).to_string();
    let sig = crypto::sign(
        &key,
        &crypto::event_message(
            "pre-fix-forgery",
            &fed.mallory.id(),
            T0 + 5,
            "mallory",
            "session.ended",
            &payload,
        ),
    );
    let hub = fed.hub.sql();
    hub.execute(
        "INSERT INTO events (event_id, node_id, ts, actor, kind, payload, prev_id, sig)
         VALUES ('pre-fix-forgery', ?1, ?2, 'mallory', 'session.ended', ?3, NULL, ?4)",
        rusqlite::params![fed.mallory.id(), T0 + 5, payload, sig],
    )
    .unwrap();
    hub.execute(
        "UPDATE agents SET status = 'offline' WHERE agent_id = ?1",
        [&bob],
    )
    .unwrap();

    // A new spoke joins and pulls everything.
    let beta = Node::new(fed._dir.path(), "beta");
    pair(&fed.hub, &beta);
    let page = fed.hub.store.pull_for(&beta.id(), 0, 10_000).unwrap();
    assert!(page.events.iter().any(|e| e.event_id == "pre-fix-forgery"));
    let report = beta
        .store
        .ingest(&fed.hub.id(), &page.events, &page.nodes)
        .unwrap();
    assert!(
        report
            .rejected
            .iter()
            .any(|r| r.event_id == "pre-fix-forgery"),
        "{report:?}"
    );
    let status: String = beta
        .sql()
        .query_row(
            "SELECT status FROM agents WHERE agent_id = ?1",
            [&bob],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "active", "the forged session end reached beta");
    // The hub itself keeps the damage: there is no repair pass.
    let hub_status: String = fed
        .hub
        .sql()
        .query_row(
            "SELECT status FROM agents WHERE agent_id = ?1",
            [&bob],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hub_status, "offline");
}

/// Control. A rejected push leaves nothing behind, so a retry after a crash (the hub
/// committed but the spoke never saw the report) is a duplicate, not a second effect.
#[test]
fn control_a_retried_push_is_idempotent() {
    let fed = Fed::new();
    let spy = fed.spy();
    let resource = fed.hub_resource();
    let payload = json!({
        "resource_id": resource, "lease_id": "retry-lease", "agent_id": spy,
        "ttl_seconds": 600, "resource_node": fed.hub.id(),
    });
    // The same signed event, delivered twice.
    fed.seq.store(42, Ordering::SeqCst);
    let first = fed.forge("resource.borrowed", payload.clone());
    fed.seq.store(42, Ordering::SeqCst);
    let second = fed.forge("resource.borrowed", payload);
    assert_eq!(
        (first.accepted, second.duplicates),
        (1, 1),
        "{first:?} {second:?}"
    );
    let leases: i64 = fed
        .hub
        .sql()
        .query_row(
            "SELECT count(*) FROM resource_leases WHERE lease_id = 'retry-lease'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(leases, 1);
}

/// Control. A revoked spoke's pushes are refused outright.
#[test]
fn control_a_revoked_spoke_cannot_push() {
    let fed = Fed::new();
    fed.hub.store.revoke_peer("mallory").unwrap();
    fed.mallory.agent("late");
    let (events, _) = fed.mallory.store.outbound_events(0, 10_000).unwrap();
    let refused = fed.hub.store.ingest(&fed.mallory.id(), &events, &[]);
    assert!(matches!(refused, Err(Error::Forbidden(_))), "{refused:?}");
}

/// Review R2-1 (P2). Each receiver bounds a foreign lease by its own clock, so a borrow
/// signed ten years ahead lives 30 minutes on the hub. The signed event is relayed
/// unchanged, though: a spoke that pulls it a week later would bound it by *its* clock and
/// give the lease a fresh 30 minutes, and the hub's earlier `resource.transferred` would
/// make that revived lease an owner there. The resource's node must publish the expiry it
/// decided, so a late observer ends the lease too. Rejecting the skewed borrow outright
/// would also satisfy this test.
#[test]
fn a_late_observer_does_not_revive_an_expired_skewed_owner() {
    let fed = Fed::new();
    let resource = fed.hub_resource();
    let spy = fed.spy();
    let report = fed.forge_at(
        T0 + 10 * 365 * DAY_MS,
        "resource.borrowed",
        json!({
            "resource_id": resource, "lease_id": "skewed-owner", "agent_id": spy,
            "ttl_seconds": 1800, "resource_node": fed.hub.id(),
        }),
    );
    let accepted = report.accepted == 1;
    if accepted {
        // The hub's owner leaves while the bounded lease is live: it is promoted.
        fed.hub
            .store
            .release_resource("alice", &ResourceRef::Id(resource.clone()), false)
            .unwrap();
        assert_eq!(
            fed.owner_node(&resource).as_deref(),
            Some(fed.mallory.id().as_str())
        );
    }

    fed.hub.advance_ms(7 * DAY_MS);
    fed.alpha.advance_ms(7 * DAY_MS);
    fed.hub.store.sweep().unwrap();
    let now = T0 + 7 * DAY_MS;
    let live = |node: &Node| -> i64 {
        node.sql()
            .query_row(
                "SELECT count(*) FROM resource_leases WHERE lease_id = 'skewed-owner'
                   AND released_at IS NULL AND expires_at > ?1",
                [now],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(live(&fed.hub), 0, "the hub still sees the lease");

    // Alpha has not synced since before the borrow; it pulls the whole history now.
    let page = fed.hub.store.pull_for(&fed.alpha.id(), 0, 10_000).unwrap();
    let applied = fed
        .alpha
        .store
        .ingest(&fed.hub.id(), &page.events, &page.nodes)
        .unwrap();
    assert!(applied.rejected.is_empty(), "{applied:?}");
    assert_eq!(
        live(&fed.alpha),
        0,
        "a late observer revived a lease the resource's node already expired"
    );
    let hub_view = fed
        .hub
        .store
        .resource(&ResourceRef::Id(resource.clone()))
        .unwrap();
    let alpha_view = fed
        .alpha
        .store
        .resource(&ResourceRef::Id(resource))
        .unwrap();
    assert_eq!(
        (hub_view.refcount, alpha_view.refcount),
        (0, 0),
        "hub and late observer disagree on the resource's live leases"
    );
}

/// The owner's expiry also reaches a spoke that only follows its own leases: the lease's
/// node is named in the event, so a spoke without `read_feed` still learns its borrow ended.
#[test]
fn the_lease_holders_node_learns_the_owner_expired_its_lease() {
    let fed = Fed::new();
    let resource = fed.hub_resource();
    let spy = fed.spy();
    let report = fed.forge(
        "resource.borrowed",
        json!({
            "resource_id": resource, "lease_id": "honest-borrow", "agent_id": spy,
            "ttl_seconds": 600, "resource_node": fed.hub.id(),
        }),
    );
    assert_eq!(report.accepted, 1, "{report:?}");
    fed.hub.advance_ms(DAY_MS);
    fed.hub.store.sweep().unwrap();
    let expiry: Vec<Value> = fed
        .hub
        .sql()
        .prepare(
            "SELECT payload FROM events WHERE node_id = ?1 AND kind = 'resource.lease_expired'",
        )
        .unwrap()
        .query_map([fed.hub.id()], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|p| serde_json::from_str(&p.unwrap()).unwrap())
        .filter(|p: &Value| p["lease_id"] == "honest-borrow")
        .collect();
    assert_eq!(
        expiry.len(),
        1,
        "the hub did not publish the foreign lease's expiry"
    );
    assert_eq!(expiry[0]["resource_nodes"], json!([fed.mallory.id()]));

    // Mallory, paired without read_feed, still receives it and ends its own lease row.
    fed.hub
        .sql()
        .execute(
            "UPDATE peers SET perms = '[\"send_mail\",\"borrow_resource\"]' WHERE node_id = ?1",
            [fed.mallory.id()],
        )
        .unwrap();
    let page = fed
        .hub
        .store
        .pull_for(&fed.mallory.id(), 0, 10_000)
        .unwrap();
    assert!(page
        .events
        .iter()
        .any(|e| e.kind == "resource.lease_expired" && e.payload.contains("honest-borrow")));
}
