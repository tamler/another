//! adv250: a paired peer's `resource.registered` carries `port` and `pid` as JSON
//! integers, which federation import stores as given. Resources are read back as
//! `port: Option<u16>` and `pid: Option<u32>`, so one out-of-range value must not make the
//! hub's resource listing fail. Every event here is correctly signed by a paired spoke.

use std::path::Path;

use another_core::test_support::forge_event;
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

/// A `resource.registered` signed by `spoke` with the given `port` and `pid`, pushed.
fn push_registered(hub: &Node, spoke: &Node, n: u64, port: Value, pid: Value) -> IngestReport {
    let node_id = spoke.store.node_id().to_string();
    let kind = "resource.registered";
    let payload = json!({
        "resource_id": format!("adv250-res-{n}"),
        "kind": "dev_server",
        "name": format!("adv250-{n}"),
        "lease_id": format!("adv250-lease-{n}"),
        "mode": "shared",
        "ttl_seconds": 600,
        "port": port,
        "pid": pid,
    })
    .to_string();
    let ev = forge_event(
        &Connection::open(&spoke.db).unwrap(),
        WireEvent {
            seq: 5_000_000 + n as i64,
            event_id: format!("adv250-ev-{n}"),
            node_id: node_id.clone(),
            ts: T0,
            actor: "spoke".into(),
            kind: kind.into(),
            payload,
            prev_id: None,
            sig: String::new(),
        },
    );
    hub.store.ingest(&node_id, &[ev], &[]).unwrap()
}

#[test]
fn adv250_an_out_of_range_peer_port_or_pid_does_not_break_the_listing() {
    let mut broken = Vec::new();
    for (n, port, pid) in [
        (1, json!(70000), json!(null)),
        (2, json!(-1), json!(null)),
        (3, json!(5173), json!(-1)),
        (4, json!(5173), json!(5_000_000_000u64)),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let hub = Node::new(dir.path(), "hub");
        let spoke = Node::new(dir.path(), "spoke");
        pair(&hub, &spoke);
        let report = push_registered(&hub, &spoke, n, port.clone(), pid.clone());
        let listed = hub.store.find_resources(ResourceFilter {
            include_released: true,
            ..Default::default()
        });
        eprintln!(
            "ADV250 case {n} port={port} pid={pid}: accepted={} listing={:?}",
            report.accepted,
            listed.as_ref().map(|l| l.len()).map_err(|e| e.to_string())
        );
        if let Err(e) = listed {
            broken.push(format!("case {n} (port {port}, pid {pid}): {e}"));
        }
    }
    assert!(
        broken.is_empty(),
        "one peer resource made the hub's resource listing fail: {broken:#?}"
    );
}

/// What the listing shows for such a resource: a value that fits is kept, one that does not
/// (it could only come from a peer) is shown as absent rather than wrapped or truncated.
#[test]
fn an_out_of_range_peer_port_or_pid_is_shown_as_absent_and_a_valid_one_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    pair(&hub, &spoke);
    // The import now refuses such values, so the bad row is what a hub stored before it
    // did: written directly, as an older build's import left it.
    push_registered(&hub, &spoke, 1, json!(1), json!(1));
    Connection::open(&hub.db)
        .unwrap()
        .execute(
            "UPDATE resources SET port = 70000, pid = -1 WHERE resource_id = 'adv250-res-1'",
            [],
        )
        .unwrap();
    push_registered(&hub, &spoke, 2, json!(5173), json!(4242));
    let listed = hub
        .store
        .find_resources(ResourceFilter {
            include_released: true,
            ..Default::default()
        })
        .unwrap();
    let by_id = |id: &str| {
        listed
            .iter()
            .find(|r| r.resource_id == id)
            .unwrap_or_else(|| panic!("{id} missing from {listed:?}"))
    };
    let bad = by_id("adv250-res-1");
    assert_eq!((bad.port, bad.pid), (None, None));
    let good = by_id("adv250-res-2");
    assert_eq!((good.port, good.pid), (Some(5173), Some(4242)));
}

/// The import refuses what no build sends: a port outside u16 or a pid outside u32, as an
/// integer of the wrong range, a negative one or not a number at all. A missing or null value
/// is still accepted (a resource need not have either).
#[test]
fn a_peer_event_with_an_out_of_range_port_or_pid_is_refused_and_not_stored() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let spoke = Node::new(dir.path(), "spoke");
    pair(&hub, &spoke);
    let stored = |n: u64| -> bool {
        Connection::open(&hub.db)
            .unwrap()
            .query_row(
                "SELECT 1 FROM resources WHERE resource_id = ?1",
                [format!("adv250-res-{n}")],
                |_| Ok(()),
            )
            .is_ok()
    };
    for (n, port, pid) in [
        (1, json!(65536), json!(null)),
        (2, json!(-1), json!(null)),
        (3, json!("80"), json!(null)),
        (4, json!(5173), json!(4_294_967_296u64)),
        (5, json!(5173), json!(-1)),
    ] {
        let report = push_registered(&hub, &spoke, n, port.clone(), pid.clone());
        assert_eq!(
            report.rejected.len(),
            1,
            "case {n} ({port}, {pid}): {report:?}"
        );
        assert!(!stored(n), "case {n} was stored");
    }
    for (n, port, pid) in [
        (6, json!(65535), json!(4_294_967_295u64)),
        (7, json!(null), json!(null)),
    ] {
        let report = push_registered(&hub, &spoke, n, port.clone(), pid.clone());
        assert!(report.rejected.is_empty(), "case {n}: {report:?}");
        assert!(stored(n), "case {n} was not stored");
    }
}
