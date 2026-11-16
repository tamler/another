//! FINDINGS-main-weak-keys: a small-order ("weak") Ed25519 public key verifies the degenerate
//! signature (R = identity, s = 0) for every message, so a node pinned with one could be
//! impersonated by anyone. Federation refuses such keys wherever a key is pinned, and checks
//! event and request signatures with Ed25519's strict rules.

use std::path::Path;

use another_core::*;
use rusqlite::{params, Connection, OptionalExtension};

/// The Ed25519 identity point, the simplest small-order public key.
fn weak_key() -> String {
    let mut bytes = [0u8; 32];
    bytes[0] = 1;
    crypto::b64(&bytes)
}

/// The signature that verifies for every message under a small-order key with the
/// non-strict rules: R = the identity point, s = 0.
fn degenerate_signature() -> String {
    let mut bytes = [0u8; 64];
    bytes[0] = 1;
    crypto::b64(&bytes)
}

struct Node {
    store: Store,
    db: std::path::PathBuf,
}

impl Node {
    fn new(dir: &Path, name: &str) -> Node {
        let home = dir.join(name);
        let store = Store::open(&home).unwrap();
        store.set_node_name(name).unwrap();
        Node {
            store,
            db: home.join(DB_FILE),
        }
    }

    fn id(&self) -> String {
        self.store.node_id().to_string()
    }

    fn pinned_key(&self, node_id: &str) -> Option<String> {
        Connection::open(&self.db)
            .unwrap()
            .query_row(
                "SELECT public_key FROM nodes WHERE node_id = ?1",
                [node_id],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
            .flatten()
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

/// An event "from" `node_id` carrying the degenerate signature.
fn forged_event(node_id: &str) -> WireEvent {
    WireEvent {
        seq: 1,
        event_id: "forged-1".into(),
        node_id: node_id.into(),
        ts: 1_800_000_000_000,
        actor: "mallory".into(),
        kind: "agent.registered".into(),
        payload: serde_json::json!({ "agent_id": "x", "name": "mallory", "program": "other" })
            .to_string(),
        prev_id: None,
        sig: degenerate_signature(),
    }
}

#[test]
fn strict_verification_refuses_a_weak_key_and_the_degenerate_signature() {
    let msg = crypto::event_message("E", "N", 1, "a", "k", "{}");
    // The non-strict check accepts it (this is the weakness).
    assert!(crypto::verify(&weak_key(), &msg, &degenerate_signature()));
    assert!(!crypto::verify_strict(
        &weak_key(),
        &msg,
        &degenerate_signature()
    ));
    // An honest signature still verifies strictly.
    let key = crypto::generate_key().unwrap();
    let sig = crypto::sign(&key, &msg);
    assert!(crypto::verify_strict(&crypto::public_of(&key), &msg, &sig));
    // A weak key is refused as a peer key; an honest one is accepted.
    assert!(crypto::decode_peer_public(&weak_key()).is_err());
    assert!(crypto::decode_peer_public(&crypto::public_of(&key)).is_ok());
}

/// A node that presents a weak key at pairing is refused, and the refusal does not use up
/// the invite: the honest request still pairs.
#[test]
fn pairing_with_a_weak_key_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    let code = hub
        .store
        .create_invite("https://hub.example", &Perm::ALL, 3600)
        .unwrap();
    let (invite, req) = spoke.store.prepare_join(&code).unwrap();
    let mut weak = req.clone();
    weak.node.public_key = weak_key();
    let err = hub.store.accept_pairing(&weak).unwrap_err();
    assert_eq!(err.code(), "invalid", "{err}");
    assert_eq!(hub.pinned_key(&spoke.id()), None);

    let resp = hub.store.accept_pairing(&req).unwrap();
    spoke.store.complete_join(&invite, &resp).unwrap();
    assert!(hub.pinned_key(&spoke.id()).is_some());
}

/// A spoke does not pin a weak key from the hub's directory, so nothing can be relayed
/// "from" that node with the degenerate signature. The entry is skipped, not fatal: the page
/// still syncs (see `adv197_...`), and the forged event fails as from an unknown origin.
#[test]
fn a_weak_key_in_the_directory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    pair(&hub, &spoke);
    let ghost = NodeEntry {
        node_id: "01GHOSTGHOSTGHOSTGHOSTGHOS".into(),
        display_name: "ghost".into(),
        public_key: weak_key(),
    };
    let res = spoke.store.ingest(
        &hub.id(),
        &[forged_event(&ghost.node_id)],
        std::slice::from_ref(&ghost),
    );
    let report = res.expect("a weak directory entry is skipped, not fatal");
    assert_eq!(report.accepted, 0, "{report:?}");
    assert_eq!(report.rejected.len(), 1, "{report:?}");
    assert!(
        report.rejected[0].reason.contains("unknown origin node"),
        "{report:?}"
    );
    assert_eq!(spoke.pinned_key(&ghost.node_id), None);
    let stored: i64 = Connection::open(&spoke.db)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM events WHERE event_id = 'forged-1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, 0);
}

/// A weak key that is already pinned (stored before this fix) no longer lets the degenerate
/// signature through: events are checked strictly.
#[test]
fn a_degenerate_signature_is_rejected_even_under_a_pinned_weak_key() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    pair(&hub, &spoke);
    let ghost = "01GHOSTGHOSTGHOSTGHOSTGHOS";
    Connection::open(&spoke.db)
        .unwrap()
        .execute(
            "INSERT INTO nodes (node_id, display_name, role, is_local, created_at, public_key)
             VALUES (?1, 'ghost', 'peer', 0, 0, ?2)",
            params![ghost, weak_key()],
        )
        .unwrap();
    let report = spoke
        .store
        .ingest(&hub.id(), &[forged_event(ghost)], &[])
        .unwrap();
    assert_eq!(report.accepted, 0, "{report:?}");
    assert_eq!(report.rejected.len(), 1, "{report:?}");
    assert!(
        report.rejected[0].reason.contains("bad event signature"),
        "{report:?}"
    );
}

/// Honest events still pass the strict check: a real spoke's history replays on the hub.
#[test]
fn honest_events_still_verify() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    pair(&hub, &spoke);
    spoke
        .store
        .register_agent(RegisterAgent {
            name: "alice".into(),
            program: Program::Claude,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    let (events, _) = spoke.store.outbound_events(0, 10_000).unwrap();
    assert!(!events.is_empty());
    let report = hub.store.ingest(&spoke.id(), &events, &[]).unwrap();
    assert!(report.rejected.is_empty(), "{report:?}");
    assert_eq!(report.accepted, events.len());
}

/// A paired spoke whose stored key is weak (pinned before this fix) cannot have requests
/// forged in its name with the degenerate signature; the spoke's real requests still pass.
#[test]
fn a_degenerate_request_signature_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    pair(&hub, &spoke);
    let now = hub.store.now();
    let body = b"{}";

    // The real spoke key signs a request that passes.
    let secret: String = Connection::open(&spoke.db)
        .unwrap()
        .query_row("SELECT secret_key FROM nodes WHERE is_local = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    let key = crypto::decode_secret(&secret).unwrap();
    let sig = crypto::sign(
        &key,
        &crypto::request_message("POST", "/fed/push", now, body),
    );
    hub.store
        .verify_peer_request(&spoke.id(), "POST", "/fed/push", now, body, &sig)
        .expect("an honest request signature");

    // With a weak key in the hub's record of the spoke, the degenerate signature is refused.
    Connection::open(&hub.db)
        .unwrap()
        .execute(
            "UPDATE nodes SET public_key = ?2 WHERE node_id = ?1",
            params![spoke.id(), weak_key()],
        )
        .unwrap();
    let err = hub
        .store
        .verify_peer_request(
            &spoke.id(),
            "POST",
            "/fed/push",
            now,
            body,
            &degenerate_signature(),
        )
        .unwrap_err();
    assert_eq!(err.code(), "forbidden", "{err}");
}

/// adv197: a hub that pinned a weak key before this fix still lists it in every pull
/// response. An upgraded spoke must keep syncing the honest events; one legacy entry in the
/// directory must not stop federation for everyone.
#[test]
fn adv197_a_legacy_weak_key_on_the_hub_does_not_stop_spokes_syncing() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    let other = Node::new(dir.path(), "other");
    pair(&hub, &spoke);
    pair(&hub, &other);

    // An honest event from another spoke reaches the hub.
    other
        .store
        .register_agent(RegisterAgent {
            name: "bob".into(),
            program: Program::Claude,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    let (events, _) = other.store.outbound_events(0, 10_000).unwrap();
    let report = hub.store.ingest(&other.id(), &events, &[]).unwrap();
    assert!(report.rejected.is_empty(), "{report:?}");

    // A weak key pinned on the hub before the upgrade.
    Connection::open(&hub.db)
        .unwrap()
        .execute(
            "INSERT INTO nodes (node_id, display_name, role, is_local, created_at, public_key)
             VALUES ('01GHOSTGHOSTGHOSTGHOSTGHOS', 'ghost', 'peer', 0, 0, ?1)",
            params![weak_key()],
        )
        .unwrap();

    let pull = hub.store.pull_for(&spoke.id(), 0, 10_000).unwrap();
    assert!(!pull.events.is_empty());
    let res = spoke.store.ingest(&hub.id(), &pull.events, &pull.nodes);
    let report = res.expect("the honest events still sync past one legacy weak key");
    assert!(report.accepted > 0, "{report:?}");
    assert_eq!(spoke.pinned_key("01GHOSTGHOSTGHOSTGHOSTGHOS"), None);
}
