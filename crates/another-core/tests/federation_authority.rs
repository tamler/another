//! A paired node signs its events, which proves only who sent them. These tests check the
//! other half: a signed event may only change objects its origin node owns. Each attack is
//! a properly signed event from a paired spoke ("mallory") that names an object belonging
//! to the hub or to another spoke ("alpha"). The hub must reject it and keep its state.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use another_core::*;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};

const T0: i64 = 1_800_000_000_000;

struct Node {
    store: Store,
    db: std::path::PathBuf,
}

impl Node {
    fn new(dir: &Path, name: &str) -> Node {
        let home = dir.join(name);
        let (clock, _) = Clock::manual(T0);
        let store = Store::open_with_clock(&home, clock).unwrap();
        store.set_node_name(name).unwrap();
        Node {
            store,
            db: home.join(DB_FILE),
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
                model: Some("original".into()),
                description: None,
            })
            .unwrap()
            .agent_id
    }

    fn id(&self) -> String {
        self.store.node_id().to_string()
    }
}

/// A hub, an honest spoke and an attacking spoke, all paired with every permission.
struct Fed {
    _dir: tempfile::TempDir,
    hub: Node,
    alpha: Node,
    mallory: Node,
    seq: AtomicU64,
}

impl Fed {
    /// An absolute project directory under the test's temporary root, on every platform.
    fn project_dir(&self, name: &str) -> String {
        let dir = self._dir.path().join("projects").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
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
    hub.store
        .ingest(&spoke.id(), &events, &[spoke.store.node_entry().unwrap()])
        .unwrap()
}

impl Fed {
    fn new() -> Fed {
        Fed::with_mallory_perms(&Perm::ALL)
    }

    /// Mallory paired with only `perms`; alpha always gets every permission.
    fn with_mallory_perms(perms: &[Perm]) -> Fed {
        let dir = tempfile::tempdir().unwrap();
        let hub = Node::new(dir.path(), "hub");
        let alpha = Node::new(dir.path(), "alpha");
        let mallory = Node::new(dir.path(), "mallory");
        pair(&hub, &alpha);
        let code = hub
            .store
            .create_invite("https://hub.example", perms, 3600)
            .unwrap();
        let (invite, req) = mallory.store.prepare_join(&code).unwrap();
        let resp = hub.store.accept_pairing(&req).unwrap();
        mallory.store.complete_join(&invite, &resp).unwrap();
        Fed {
            _dir: dir,
            hub,
            alpha,
            mallory,
            seq: AtomicU64::new(0),
        }
    }

    /// A correctly signed event from mallory, pushed to the hub.
    fn forge(&self, kind: &str, payload: Value) -> IngestReport {
        let secret: String = self
            .mallory
            .sql()
            .query_row("SELECT secret_key FROM nodes WHERE is_local = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        let key = crypto::decode_secret(&secret).unwrap();
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let payload = payload.to_string();
        let node_id = self.mallory.id();
        let event_id = format!("forged-{kind}-{n}");
        let ts = T0 + 1000 + n as i64;
        let sig = crypto::sign(
            &key,
            &crypto::event_message(&event_id, &node_id, ts, "mallory", kind, &payload),
        );
        let ev = WireEvent {
            seq: 1_000_000 + n as i64,
            event_id,
            node_id,
            ts,
            actor: "mallory".into(),
            kind: kind.into(),
            payload,
            prev_id: None,
            sig,
        };
        self.hub
            .store
            .ingest(
                &self.mallory.id(),
                &[ev],
                &[self.mallory.store.node_entry().unwrap()],
            )
            .unwrap()
    }

    fn hub_one<T: rusqlite::types::FromSql>(&self, sql: &str, arg: &str) -> Option<T> {
        self.hub
            .sql()
            .query_row(sql, [arg], |r| r.get(0))
            .optional()
            .unwrap()
    }

    /// A resource owned by hub agent `alice`, borrowed by hub agent `bob`.
    fn hub_resource(&self) -> (String, String, String) {
        let owned = self
            .hub
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
            .unwrap();
        let borrowed = self
            .hub
            .store
            .borrow_resource(
                "bob",
                &ResourceRef::Id(owned.resource.resource_id.clone()),
                None,
            )
            .unwrap();
        (
            owned.resource.resource_id,
            owned.lease.unwrap().lease_id,
            borrowed.lease.unwrap().lease_id,
        )
    }
}

/// The event must be refused and reported. Set `FED_STATE_ONLY=1` to skip this and see
/// only the state checks, which is how the gap is shown on an unfixed build.
fn assert_rejected(report: &IngestReport, what: &str) {
    if std::env::var_os("FED_STATE_ONLY").is_some() {
        return;
    }
    assert_eq!(report.accepted, 0, "{what}: accepted {report:?}");
    assert_eq!(report.rejected.len(), 1, "{what}: not reported {report:?}");
}

#[test]
fn agent_registration_cannot_rewrite_another_nodes_agent() {
    let fed = Fed::new();
    let alice = fed.hub.agent("alice");
    let carol = fed.alpha.agent("carol");
    push(&fed.hub, &fed.alpha);
    for victim in [&alice, &carol] {
        let before: String = fed
            .hub_one("SELECT address FROM agents WHERE agent_id = ?1", victim)
            .unwrap();
        let report = fed.forge(
            "agent.registered",
            json!({ "agent_id": victim, "name": "evil", "program": "other", "model": "evil" }),
        );
        assert_rejected(&report, "agent.registered");
        let (node, model, address): (String, Option<String>, String) = fed
            .hub
            .sql()
            .query_row(
                "SELECT node_id, model, address FROM agents WHERE agent_id = ?1",
                [victim],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_ne!(node, fed.mallory.id());
        assert_eq!(model.as_deref(), Some("original"));
        assert_eq!(address, before);
    }
}

#[test]
fn session_events_cannot_change_another_nodes_agent() {
    let fed = Fed::new();
    let alice = fed.hub.agent("alice");
    let report = fed.forge("session.ended", json!({ "agent_id": alice }));
    assert_rejected(&report, "session.ended");
    let status: String = fed
        .hub_one("SELECT status FROM agents WHERE agent_id = ?1", &alice)
        .unwrap();
    assert_eq!(status, "active");
}

#[test]
fn mail_cannot_claim_another_nodes_sender() {
    let fed = Fed::new();
    let alice = fed.hub.agent("alice");
    let bob = fed.hub.agent("bob");
    let report = fed.forge(
        "message.sent",
        json!({
            "message_id": "forged-message", "sender_id": alice, "subject": "from alice",
            "body": "trust me", "recipient_nodes": [fed.hub.id()],
            "recipients": [{ "agent_id": bob, "node_id": fed.hub.id(), "role": "to" }],
        }),
    );
    assert_rejected(&report, "message.sent");
    assert_eq!(
        fed.hub_one::<String>(
            "SELECT sender_id FROM messages WHERE message_id = ?1",
            "forged-message"
        ),
        None
    );
}

#[test]
fn mail_cannot_add_recipients_to_another_nodes_message() {
    let fed = Fed::new();
    fed.hub.agent("alice");
    fed.hub.agent("bob");
    let spy = fed.mallory.agent("spy");
    push(&fed.hub, &fed.mallory);
    let private = fed
        .hub
        .store
        .send_message(SendMessage {
            from: "alice".into(),
            to: vec!["bob".into()],
            subject: "private".into(),
            body: "between hub agents".into(),
            ..Default::default()
        })
        .unwrap()
        .message_id;
    let report = fed.forge(
        "message.sent",
        json!({
            "message_id": private, "sender_id": spy, "subject": "x", "body": "x",
            "recipient_nodes": [fed.hub.id()],
            "recipients": [{ "agent_id": spy, "node_id": fed.mallory.id(), "role": "cc" }],
        }),
    );
    assert_rejected(&report, "message.sent reusing an id");
    let leaked: i64 = fed
        .hub
        .sql()
        .query_row(
            "SELECT count(*) FROM deliveries WHERE message_id = ?1 AND agent_id = ?2",
            [&private, &spy],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(leaked, 0);
}

#[test]
fn read_and_ack_cannot_touch_another_nodes_delivery() {
    let fed = Fed::new();
    fed.hub.agent("alice");
    let bob = fed.hub.agent("bob");
    let message = fed
        .hub
        .store
        .send_message(SendMessage {
            from: "alice".into(),
            to: vec!["bob".into()],
            subject: "unread".into(),
            body: "b".into(),
            ..Default::default()
        })
        .unwrap()
        .message_id;
    for kind in ["delivery.read", "delivery.acked"] {
        let report = fed.forge(
            kind,
            json!({ "message_id": message, "agent_id": bob, "message_node": fed.hub.id() }),
        );
        assert_rejected(&report, kind);
    }
    let (read, acked): (Option<i64>, Option<i64>) = fed
        .hub
        .sql()
        .query_row(
            "SELECT read_at, acked_at FROM deliveries WHERE message_id = ?1 AND agent_id = ?2",
            [&message, &bob],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((read, acked), (None, None));
}

#[test]
fn transfer_cannot_promote_a_lease_on_another_nodes_resource() {
    let fed = Fed::new();
    fed.hub.agent("alice");
    let bob = fed.hub.agent("bob");
    let (rid, _, borrower) = fed.hub_resource();
    let report = fed.forge(
        "resource.transferred",
        json!({ "resource_id": rid, "lease_id": borrower, "owner_id": bob }),
    );
    assert_rejected(&report, "resource.transferred");
    let role: String = fed
        .hub_one(
            "SELECT role FROM resource_leases WHERE lease_id = ?1",
            &borrower,
        )
        .unwrap();
    assert_eq!(role, "borrower");
}

#[test]
fn stop_cannot_end_leases_on_another_nodes_resource() {
    let fed = Fed::new();
    fed.hub.agent("alice");
    fed.hub.agent("bob");
    let (rid, owner, borrower) = fed.hub_resource();
    for kind in ["resource.stopped", "resource.reaped"] {
        let report = fed.forge(kind, json!({ "resource_id": rid }));
        assert_rejected(&report, kind);
    }
    let live: i64 = fed
        .hub
        .sql()
        .query_row(
            "SELECT count(*) FROM resource_leases WHERE lease_id IN (?1, ?2)
             AND released_at IS NULL",
            [&owner, &borrower],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(live, 2, "the hub's own leases were ended by a foreign stop");
}

#[test]
fn registration_and_adoption_cannot_take_another_nodes_resource() {
    let fed = Fed::new();
    fed.hub.agent("alice");
    fed.hub.agent("bob");
    let spy = fed.mallory.agent("spy");
    push(&fed.hub, &fed.mallory);
    let (rid, _, _) = fed.hub_resource();
    let report = fed.forge(
        "resource.registered",
        json!({ "resource_id": rid, "kind": "model_server", "name": "ollama@11434",
                "lease_id": "forged-owner", "owner_id": spy }),
    );
    assert_rejected(&report, "resource.registered with a taken id");
    let report = fed.forge(
        "resource.adopted",
        json!({ "resource_id": rid, "lease_id": "forged-adopt", "agent_id": spy }),
    );
    assert_rejected(&report, "resource.adopted");
    let stolen: i64 = fed
        .hub
        .sql()
        .query_row(
            "SELECT count(*) FROM resource_leases WHERE resource_id = ?1 AND agent_id = ?2",
            [&rid, &spy],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stolen, 0);
}

#[test]
fn leases_and_holds_cannot_name_another_nodes_agent() {
    let fed = Fed::new();
    let alice = fed.hub.agent("alice");
    fed.hub.agent("bob");
    let (rid, _, _) = fed.hub_resource();
    let report = fed.forge(
        "resource.borrowed",
        json!({ "resource_id": rid, "lease_id": "forged-borrow", "agent_id": alice,
                "resource_node": fed.hub.id() }),
    );
    assert_rejected(&report, "resource.borrowed for a hub agent");
    assert_eq!(
        fed.hub_one::<String>(
            "SELECT agent_id FROM resource_leases WHERE lease_id = ?1",
            "forged-borrow"
        ),
        None
    );
    // A resource mallory really registers, with a hub agent named as its owner.
    let report = fed.forge(
        "resource.registered",
        json!({ "resource_id": "mallory-res", "kind": "port", "name": "tcp:1",
                "lease_id": "forged-owner-lease", "owner_id": alice }),
    );
    assert_rejected(&report, "resource.registered owned by a hub agent");
    // A hold in a project mallory owns, attributed to a hub agent.
    let project = fed
        .mallory
        .store
        .ensure_project("test", &fed.project_dir("mallory-shop"))
        .unwrap();
    push(&fed.hub, &fed.mallory);
    let report = fed.forge(
        "hold.acquired",
        json!({ "hold_id": "forged-hold", "project_id": project.project_id, "agent_id": alice,
                "pattern": "src/a.rs", "exclusive": true, "ttl_seconds": 1800 }),
    );
    assert_rejected(&report, "hold.acquired for a hub agent");
    assert_eq!(
        fed.hub_one::<String>(
            "SELECT agent_id FROM holds WHERE hold_id = ?1",
            "forged-hold"
        ),
        None
    );
}

/// The checks must not reject what an honest node really sends about its own objects.
#[test]
fn a_nodes_own_events_still_sync() {
    let fed = Fed::new();
    fed.hub.agent("alice");
    let bob = fed.hub.agent("bob");
    let (rid, _, _) = fed.hub_resource();
    let carol = fed.alpha.agent("carol");
    let project = fed
        .alpha
        .store
        .ensure_project("test", &fed.project_dir("alpha-shop"))
        .unwrap();
    fed.alpha.store.heartbeat("carol", true).unwrap();
    fed.alpha
        .store
        .session("carol", SessionPhase::Start, SessionInfo::default())
        .unwrap();
    // Alpha learns the hub's agents and resources, then mails and borrows across nodes.
    let pulled = fed.hub.store.pull_for(&fed.alpha.id(), 0, 10_000).unwrap();
    fed.alpha
        .store
        .ingest(&fed.hub.id(), &pulled.events, &pulled.nodes)
        .unwrap();
    fed.alpha
        .store
        .send_message(SendMessage {
            from: "carol".into(),
            to: vec!["bob@@hub".into()],
            subject: "hello".into(),
            body: "from alpha".into(),
            ..Default::default()
        })
        .unwrap();
    fed.alpha
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
        .unwrap();
    fed.alpha
        .store
        .register_resource(RegisterResource {
            agent: "carol".into(),
            kind: ResourceKind::Port,
            name: "tcp:127.0.0.1:1".into(),
            endpoint: None,
            port: Some(1),
            pid: None,
            project: None,
            purpose: None,
            mode: LeaseMode::Shared,
            ttl_seconds: None,
            metadata: None,
        })
        .unwrap();
    let report = push(&fed.hub, &fed.alpha);
    assert!(
        report.rejected.is_empty(),
        "honest events were rejected: {:?}",
        report.rejected
    );
    let hub = fed.hub.sql();
    let count = |sql: &str, arg: &str| -> i64 { hub.query_row(sql, [arg], |r| r.get(0)).unwrap() };
    assert_eq!(
        count(
            "SELECT count(*) FROM agents WHERE agent_id = ?1 AND status = 'active'",
            &carol
        ),
        1
    );
    assert_eq!(
        count(
            "SELECT count(*) FROM deliveries d JOIN messages m ON m.message_id = d.message_id
             WHERE m.sender_id = ?1",
            &carol
        ),
        1
    );
    assert_eq!(
        count("SELECT count(*) FROM holds WHERE agent_id = ?1", &carol),
        1
    );
    assert_eq!(
        count(
            "SELECT count(*) FROM resource_leases WHERE agent_id = ?1 AND role = 'owner'",
            &carol
        ),
        1
    );
    // The hub's own resource is untouched by alpha's sync.
    assert_eq!(
        count(
            "SELECT count(*) FROM resource_leases WHERE resource_id = ?1 AND released_at IS NULL",
            &rid
        ),
        2
    );
    // Bob reads alpha's mail on the hub; the hub's read event is accepted on alpha.
    let inbox = fed.hub.store.inbox("bob", InboxQuery::default()).unwrap();
    let from_carol: Vec<String> = inbox
        .messages
        .iter()
        .filter(|m| m.message.subject == "hello")
        .map(|m| m.message.message_id.clone())
        .collect();
    assert_eq!(from_carol.len(), 1);
    fed.hub.store.acknowledge("bob", &from_carol, true).unwrap();
    let pulled = fed.hub.store.pull_for(&fed.alpha.id(), 0, 10_000).unwrap();
    let report = fed
        .alpha
        .store
        .ingest(&fed.hub.id(), &pulled.events, &pulled.nodes)
        .unwrap();
    assert!(report.rejected.is_empty(), "{:?}", report.rejected);
    let acked: Option<i64> = fed
        .alpha
        .sql()
        .query_row(
            "SELECT acked_at FROM deliveries WHERE message_id = ?1 AND agent_id = ?2",
            [&from_carol[0], &bob],
            |r| r.get(0),
        )
        .unwrap();
    assert!(acked.is_some(), "bob's ack reached alpha");
}

/// Review P1: whether a borrow needs `borrow_resource` must follow the resource the hub
/// actually knows, never the routing hint the event's author chose. Every hint variant is
/// refused for a spoke without the permission; the correctly labelled one is the control.
#[test]
fn a_spoke_without_borrow_permission_cannot_borrow_whatever_the_routing_hint() {
    let fed = Fed::with_mallory_perms(&[Perm::ReadFeed]);
    fed.hub.agent("alice");
    fed.hub.agent("bob");
    let spy = fed.mallory.agent("spy");
    assert!(push(&fed.hub, &fed.mallory).rejected.is_empty());
    let (rid, _, _) = fed.hub_resource();
    let variants = [
        ("labelled", json!({ "resource_node": fed.hub.id() })),
        ("missing", json!({})),
        (
            "self-labelled",
            json!({ "resource_node": fed.mallory.id() }),
        ),
        ("wrong node", json!({ "resource_node": fed.alpha.id() })),
        ("empty list", json!({ "resource_nodes": [] })),
    ];
    for (i, (what, hint)) in variants.into_iter().enumerate() {
        let mut payload = json!({
            "resource_id": rid, "lease_id": format!("lease-{i}"), "agent_id": spy,
            "ttl_seconds": 600,
        });
        for (k, v) in hint.as_object().unwrap() {
            payload[k] = v.clone();
        }
        let report = fed.forge("resource.borrowed", payload);
        assert_rejected(&report, &format!("borrow with a {what} hint"));
        assert_eq!(
            fed.hub_one::<String>(
                "SELECT lease_id FROM resource_leases WHERE lease_id = ?1",
                &format!("lease-{i}")
            ),
            None,
            "{what} hint created a lease"
        );
    }
    // A resource the hub does not know cannot be proved to be mallory's own.
    let report = fed.forge(
        "resource.borrowed",
        json!({ "resource_id": "unknown-res", "lease_id": "lease-unknown", "agent_id": spy }),
    );
    assert_rejected(&report, "borrow of an unknown resource");
}

/// The permission check leaves borrowing one's own resource alone.
#[test]
fn a_spoke_without_borrow_permission_still_syncs_its_own_borrows() {
    let fed = Fed::with_mallory_perms(&[Perm::ReadFeed]);
    fed.mallory.agent("owner");
    fed.mallory.agent("user");
    let rid = fed
        .mallory
        .store
        .register_resource(RegisterResource {
            agent: "owner".into(),
            kind: ResourceKind::Port,
            name: "tcp:127.0.0.1:1".into(),
            endpoint: None,
            port: Some(1),
            pid: None,
            project: None,
            purpose: None,
            mode: LeaseMode::Shared,
            ttl_seconds: None,
            metadata: None,
        })
        .unwrap()
        .resource
        .resource_id;
    fed.mallory
        .store
        .borrow_resource("user", &ResourceRef::Id(rid.clone()), None)
        .unwrap();
    let report = push(&fed.hub, &fed.mallory);
    assert!(report.rejected.is_empty(), "{:?}", report.rejected);
    let leases: i64 = fed
        .hub
        .sql()
        .query_row(
            "SELECT count(*) FROM resource_leases WHERE resource_id = ?1",
            [&rid],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(leases, 2);
}

/// F3 follow-up: when a receiver already knows two nodes by one name (state from before
/// renames were checked), an address by that name is refused instead of picking one.
#[test]
fn an_address_matching_two_nodes_is_ambiguous() {
    let fed = Fed::new();
    fed.alpha.agent("bob");
    fed.mallory.agent("bob");
    assert!(push(&fed.hub, &fed.alpha).rejected.is_empty());
    assert!(push(&fed.hub, &fed.mallory).rejected.is_empty());
    fed.hub
        .sql()
        .execute(
            "UPDATE nodes SET display_name = 'alpha' WHERE node_id = ?1",
            [fed.mallory.id()],
        )
        .unwrap();
    fed.hub.agent("carol");
    let sent = fed.hub.store.send_message(SendMessage {
        from: "carol".into(),
        to: vec!["bob@@alpha".into()],
        subject: "which bob?".into(),
        body: "b".into(),
        ..Default::default()
    });
    assert!(matches!(sent, Err(Error::Ambiguous(_))), "{sent:?}");
    // The node id still names one of them exactly.
    let exact = fed.hub.store.send_message(SendMessage {
        from: "carol".into(),
        to: vec![format!("bob@@{}", fed.alpha.id())],
        subject: "alpha's bob".into(),
        body: "b".into(),
        ..Default::default()
    });
    assert!(exact.is_ok(), "{exact:?}");
}

/// F1 follow-up: the hub's release of a revoked spoke's lease reaches the other spokes,
/// and none of them sees the revoked node become the owner.
#[test]
fn a_revoked_spokes_lease_release_reaches_other_spokes() {
    let fed = Fed::new();
    fed.hub.agent("alice");
    fed.hub.agent("bob");
    let spy = fed.mallory.agent("spy");
    assert!(push(&fed.hub, &fed.mallory).rejected.is_empty());
    let (rid, owner, _) = fed.hub_resource();
    let report = fed.forge(
        "resource.borrowed",
        json!({ "resource_id": rid, "lease_id": "spy-lease", "agent_id": spy,
                "ttl_seconds": 1800, "resource_node": fed.hub.id() }),
    );
    assert_eq!(report.accepted, 1, "{report:?}");
    let pull = |node: &Node| {
        let page = fed.hub.store.pull_for(&node.id(), 0, 10_000).unwrap();
        let report = node
            .store
            .ingest(&fed.hub.id(), &page.events, &page.nodes)
            .unwrap();
        assert!(report.rejected.is_empty(), "{:?}", report.rejected);
    };
    pull(&fed.alpha);
    let live = |node: &Node, lease: &str| -> bool {
        node.sql()
            .query_row(
                "SELECT released_at IS NULL FROM resource_leases WHERE lease_id = ?1",
                [lease],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert!(live(&fed.alpha, "spy-lease"), "alpha saw the borrow");

    fed.hub.store.revoke_peer("mallory").unwrap();
    assert!(
        !live(&fed.hub, "spy-lease"),
        "revoking ended the lease on the hub"
    );
    fed.hub
        .store
        .release_resource("alice", &ResourceRef::Id(rid.clone()), false)
        .unwrap();
    pull(&fed.alpha);
    assert!(!live(&fed.alpha, "spy-lease"), "the release reached alpha");
    assert!(!live(&fed.alpha, &owner), "alice's release reached alpha");
    let mallory_owner: i64 = fed
        .alpha
        .sql()
        .query_row(
            "SELECT count(*) FROM resource_leases WHERE resource_id = ?1 AND node_id = ?2
             AND role = 'owner'",
            [&rid, &fed.mallory.id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(mallory_owner, 0);
}

/// adv248: #248 applies the local node-name rule to a peer's node list. A peer can also
/// rename itself with a signed `node.renamed` event, which checks only the taken rule, so a
/// name a local `another node name` refuses (oversized, an `@`, a space) still lands.
/// `node.created` carries a name the same way, so it follows the same rule.
#[test]
fn adv248_a_peers_node_renamed_follows_the_local_name_rule() {
    let fed = Fed::new();
    let mut kept = Vec::new();
    for kind in ["node.renamed", "node.created"] {
        for name in [
            "x".repeat(100_000),
            "evil@hub".to_string(),
            "two words".to_string(),
        ] {
            let report = fed.forge(
                kind,
                json!({ "node_id": fed.mallory.id(), "display_name": name }),
            );
            assert_eq!(report.rejected.len(), 1, "{kind} {name:.20}: {report:?}");
            let stored: Option<String> = fed.hub_one(
                "SELECT display_name FROM nodes WHERE node_id = ?1",
                &fed.mallory.id(),
            );
            if stored.as_deref() == Some(name.as_str()) {
                kept.push(format!("{kind} {:.20} ({} bytes)", name, name.len()));
            }
        }
    }
    assert!(
        kept.is_empty(),
        "the hub renamed a peer to names a local rename refuses: {kept:#?}"
    );
}
