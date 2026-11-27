//! What a hub refuses from a spoke before looking at a signature: events another node
//! authored (even correctly signed by that node), and events that are never shared (token,
//! peer and OAuth records, mail with no recipient on another node, read receipts for a
//! local message). An honest spoke never sends these, since `outbound_events` filters them,
//! so these tests send them the way a modified spoke could.

use std::path::Path;

use another_core::test_support::wire_events_of;
use another_core::*;
use rusqlite::Connection;

struct Node {
    store: Store,
    db: std::path::PathBuf,
}

impl Node {
    fn new(dir: &Path, name: &str) -> Node {
        let home = dir.join(name);
        let store = Store::open(&home).unwrap().unguarded_repair();
        store.set_node_name(name).unwrap();
        Node {
            store,
            db: home.join(DB_FILE),
        }
    }

    fn id(&self) -> String {
        self.store.node_id().to_string()
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

    /// This node's own stored events of these kinds, exactly as signed.
    fn signed(&self, kinds: &[&str]) -> Vec<WireEvent> {
        wire_events_of(&self.sql(), &self.id())
            .into_iter()
            .filter(|e| kinds.contains(&e.kind.as_str()))
            .collect()
    }

    fn has_event(&self, event_id: &str) -> bool {
        self.sql()
            .query_row(
                "SELECT count(*) FROM events WHERE event_id = ?1",
                [event_id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            > 0
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

fn all_rejected(report: &IngestReport, n: usize, reason: &str) {
    assert_eq!(report.accepted, 0, "{report:?}");
    assert_eq!(report.rejected.len(), n, "{report:?}");
    for r in &report.rejected {
        assert!(r.reason.contains(reason), "{report:?}");
    }
}

/// A spoke that relays another spoke's genuine, correctly signed events is refused, and the
/// relay does not use them up: the author's own push still lands.
#[test]
fn a_spoke_cannot_push_events_another_node_authored() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let alpha = Node::new(dir.path(), "alpha");
    let mallory = Node::new(dir.path(), "mallory");
    pair(&hub, &alpha);
    pair(&hub, &mallory);
    alpha.agent("bob");
    let (alphas, _) = alpha.store.outbound_events(0, 10_000).unwrap();
    let bob = alphas
        .iter()
        .find(|e| e.kind == "agent.registered")
        .unwrap()
        .event_id
        .clone();

    let relayed = hub
        .store
        .ingest(&mallory.id(), &alphas, &[alpha.store.node_entry().unwrap()])
        .unwrap();
    all_rejected(
        &relayed,
        alphas.len(),
        "spokes may only push events they authored",
    );
    assert!(!hub.has_event(&bob));

    let honest = hub
        .store
        .ingest(&alpha.id(), &alphas, &[alpha.store.node_entry().unwrap()])
        .unwrap();
    assert!(honest.rejected.is_empty(), "{honest:?}");
    assert!(hub.has_event(&bob));
}

/// A spoke's own token and peer records, local-only mail and receipts for that mail stay on
/// the spoke, even when it pushes them correctly signed.
#[test]
fn a_spoke_cannot_push_events_that_are_never_shared() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let mallory = Node::new(dir.path(), "mallory");
    pair(&hub, &mallory);
    mallory.agent("carol");
    mallory.agent("dave");
    mallory
        .store
        .create_token("carol", &["**".to_string()], None, None)
        .unwrap();
    let local = mallory
        .store
        .send_message(SendMessage {
            from: "carol".into(),
            to: vec!["dave".into()],
            subject: "local only".into(),
            body: "b".into(),
            ..Default::default()
        })
        .unwrap();
    mallory
        .store
        .acknowledge("dave", std::slice::from_ref(&local.message_id), true)
        .unwrap();

    let private = mallory.signed(&[
        "token.created",
        "peer.paired",
        "message.sent",
        "delivery.acked",
    ]);
    let kinds: std::collections::BTreeSet<_> = private.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(
        kinds.into_iter().collect::<Vec<_>>(),
        [
            "delivery.acked",
            "message.sent",
            "peer.paired",
            "token.created"
        ],
        "the fixture produced each private kind"
    );
    // An honest push leaves them out.
    let (outbound, _) = mallory.store.outbound_events(0, 10_000).unwrap();
    for e in &private {
        assert!(
            outbound.iter().all(|o| o.event_id != e.event_id),
            "{} was offered",
            e.kind
        );
    }

    let report = hub
        .store
        .ingest(
            &mallory.id(),
            &private,
            &[mallory.store.node_entry().unwrap()],
        )
        .unwrap();
    all_rejected(&report, private.len(), "events are not shared");
    for e in &private {
        assert!(!hub.has_event(&e.event_id), "{} was stored", e.kind);
    }
    let messages: i64 = hub
        .sql()
        .query_row(
            "SELECT count(*) FROM messages WHERE message_id = ?1",
            [&local.message_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(messages, 0);
}
