//! A local agent name must be 1-64 ASCII letters, digits, '-', '_' or '.' (`identity.rs`
//! `validate_name`). A paired peer's `agent.registered` is held to the same rule: the hub
//! rejects it instead of storing an oversized, look-alike or unaddressable name
//! (FINDINGS-main-peer-agent-name-w9Y; the failing test was its finder's). Every event here is
//! correctly signed by a paired spoke.

use std::path::Path;

use another_core::*;
use rusqlite::Connection;
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

/// An `agent.registered` signed by `spoke` for an agent called `name`, pushed to `hub`.
fn push_agent(hub: &Node, spoke: &Node, n: u64, name: &str) -> IngestReport {
    push_payload(
        hub,
        spoke,
        n,
        json!({
            "agent_id": format!("adv-agent-{n}"),
            "name": name,
            "program": "claude",
            "new": true,
        }),
    )
}

/// An `agent.registered` with this exact payload, signed by `spoke` and pushed to `hub`.
fn push_payload(hub: &Node, spoke: &Node, n: u64, payload: Value) -> IngestReport {
    let secret: String = Connection::open(&spoke.db)
        .unwrap()
        .query_row("SELECT secret_key FROM nodes WHERE is_local = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    let key = crypto::decode_secret(&secret).unwrap();
    let node_id = spoke.store.node_id().to_string();
    let kind = "agent.registered";
    let payload = payload.to_string();
    let event_id = format!("adv-agent-ev-{n}");
    let sig = crypto::sign(
        &key,
        &crypto::event_message(&event_id, &node_id, T0, "spoke", kind, &payload),
    );
    let ev = WireEvent {
        seq: 4_000_000 + n as i64,
        event_id,
        node_id: node_id.clone(),
        ts: T0,
        actor: "spoke".into(),
        kind: kind.into(),
        payload,
        prev_id: None,
        sig,
    };
    hub.store.ingest(&node_id, &[ev], &[]).unwrap()
}

/// What the hub stored for agent `adv-agent-{n}`: (name byte length, address).
fn stored(hub: &Node, n: u64) -> Option<(usize, String)> {
    Connection::open(&hub.db)
        .unwrap()
        .query_row(
            "SELECT length(CAST(name AS BLOB)), address FROM agents WHERE agent_id = ?1",
            [format!("adv-agent-{n}")],
            |r| Ok((r.get::<_, i64>(0)? as usize, r.get::<_, String>(1)?)),
        )
        .ok()
}

/// The local rule, as `identity.rs` `validate_name` states it.
fn locally_valid(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

#[test]
fn a_peer_agent_name_follows_the_local_name_rule() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    pair(&hub, &spoke);

    let long = "x".repeat(100_000);
    let cases: [(u64, &str); 5] = [
        (1, &long),
        (2, "\u{430}lice"), // Cyrillic a: reads as "alice"
        (3, "alice@hub"),   // '@' is the address separator
        (4, "two words"),
        (5, ""),
    ];
    let mut kept = Vec::new();
    for (n, name) in cases {
        assert!(!locally_valid(name), "case {n} must be locally invalid");
        let report = push_agent(&hub, &spoke, n, name);
        assert_eq!(
            report.rejected.len(),
            1,
            "case {n} was not refused: {report:?}"
        );
        if let Some((len, address)) = stored(&hub, n) {
            kept.push(format!(
                "case {n}: {len}-byte name, address {:.40}",
                address
            ));
        }
    }
    assert!(
        kept.is_empty(),
        "the hub stored peer agents whose names a local registration refuses: {kept:#?}"
    );

    // A name the local rule accepts still arrives, with its peer address.
    let report = push_agent(&hub, &spoke, 6, "carol.v2-test_1");
    assert!(report.rejected.is_empty(), "{report:?}");
    assert_eq!(
        stored(&hub, 6).map(|(_, address)| address),
        Some("carol.v2-test_1@@spoke".to_string())
    );
}

/// A payload with no `name` at all is refused too: the projection would otherwise store an
/// agent with an empty name at `@@spoke`, like the empty-name case above.
#[test]
fn a_peer_agent_without_a_name_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    pair(&hub, &spoke);

    let report = push_payload(
        &hub,
        &spoke,
        7,
        json!({ "agent_id": "adv-agent-7", "program": "claude", "new": true }),
    );
    assert_eq!(
        report.rejected.len(),
        1,
        "a nameless peer agent was not refused: {report:?}"
    );
    assert_eq!(
        stored(&hub, 7),
        None,
        "the hub stored a nameless peer agent"
    );
}
