//! Option A: the fixed hub decides whether a borrow or renewal was in time, once, when the
//! borrower pushes it. What it relays is replayed without a clock check, including by a resource
//! owner that was offline. Three in-memory nodes with clocks that move together, and an ephemeral
//! listener for reachability.

use std::net::TcpListener;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use another_core::*;

const START: i64 = 1_800_000_000_000;
const MINUTE: i64 = 60_000;
const BORROWER: &str = "worker@@borrower";

fn node(name: &str, now: i64) -> (Store, Arc<AtomicI64>) {
    let (clock, time) = Clock::manual(now);
    let store = Store::open_in_memory(clock).unwrap();
    store.set_node_name(name).unwrap();
    store
        .register_agent_trusted(RegisterAgent {
            name: "worker".into(),
            program: Program::Codex,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    (store, time)
}

fn pair(hub: &Store, spoke: &Store) {
    let code = hub
        .create_invite("http://hub.test", &Perm::ALL, 86_400)
        .unwrap();
    let (invite, request) = spoke.prepare_join(&code).unwrap();
    let response = hub.accept_pairing(&request).unwrap();
    spoke.complete_join(&invite, &response).unwrap();
}

fn push(hub: &Store, spoke: &Store) -> IngestReport {
    let (events, _) = spoke.outbound_events(0, 2000).unwrap();
    hub.ingest(spoke.node_id(), &events, &[]).unwrap()
}

fn pull(hub: &Store, spoke: &Store) {
    let page = hub.pull_for(spoke.node_id(), 0, 2000).unwrap();
    let report = spoke
        .ingest(hub.node_id(), &page.events, &page.nodes)
        .unwrap();
    assert!(report.rejected.is_empty(), "pull: {report:?}");
}

fn sync(hub: &Store, spoke: &Store) {
    let report = push(hub, spoke);
    assert!(report.rejected.is_empty(), "push: {report:?}");
    pull(hub, spoke);
}

fn kinds(store: &Store, kind: &str) -> usize {
    store
        .events(20_000)
        .unwrap()
        .iter()
        .filter(|e| e.kind == kind)
        .count()
}

struct Net {
    hub: Store,
    owner: Store,
    borrower: Store,
    clocks: Vec<Arc<AtomicI64>>,
    endpoint: String,
    _listener: TcpListener,
}

impl Net {
    fn new() -> Net {
        let (hub, h) = node("hub", START);
        let (owner, o) = node("owner", START);
        let (borrower, b) = node("borrower", START);
        pair(&hub, &owner);
        pair(&hub, &borrower);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        Net {
            hub,
            owner,
            borrower,
            clocks: vec![h, o, b],
            endpoint,
            _listener: listener,
        }
    }

    fn at(&self, minutes: i64) {
        for clock in &self.clocks {
            clock.store(START + minutes * MINUTE, Ordering::SeqCst);
        }
    }

    /// A shared resource on the owner with a one-hour owner lease, known to every node.
    fn resource(&self, name: &str) -> (ResourceRef, String) {
        let r = self
            .owner
            .register_resource(RegisterResource {
                agent: "worker".into(),
                kind: ResourceKind::ModelServer,
                name: name.into(),
                endpoint: Some(self.endpoint.clone()),
                port: None,
                pid: None,
                project: None,
                purpose: None,
                mode: LeaseMode::Shared,
                ttl_seconds: Some(3600),
                metadata: None,
            })
            .unwrap()
            .resource;
        sync(&self.hub, &self.owner);
        sync(&self.hub, &self.borrower);
        (ResourceRef::Id(r.resource_id.clone()), r.resource_id)
    }

    fn borrow(&self, id: &ResourceRef) {
        self.borrower
            .borrow_resource("worker", id, Some(1800))
            .unwrap();
        sync(&self.hub, &self.borrower);
    }

    /// Borrow at 0 and renew at 10, 20 and 35 minutes, all admitted while live; the owner hears
    /// none of it. The lease then runs to 65 minutes.
    fn renewed_to_65(&self, id: &ResourceRef) {
        self.borrow(id);
        for minutes in [10, 20, 35] {
            self.at(minutes);
            self.borrow(id);
        }
    }
}

#[test]
fn a_delayed_owner_honours_renewals_the_hub_admitted() {
    let net = Net::new();
    let (id, _) = net.resource("shared");
    net.renewed_to_65(&id);

    net.at(40);
    sync(&net.hub, &net.owner);
    assert_eq!(net.hub.resource(&id).unwrap().refcount, 2);
    assert_eq!(net.owner.resource(&id).unwrap().refcount, 2);

    net.at(61);
    net.owner.sweep().unwrap();
    let after = net.owner.resource(&id).unwrap();
    assert_eq!(after.refcount, 1);
    assert_eq!(after.status, "live");
    assert_eq!(after.owner.as_deref(), Some(BORROWER));

    // The promotion is the owner's signed event, so the hub learns it from there.
    sync(&net.hub, &net.owner);
    assert_eq!(
        net.hub.resource(&id).unwrap().owner.as_deref(),
        Some(BORROWER)
    );
}

/// The owner stays offline past its own lease: no sync and no sweep until minute 62. The borrower's
/// first lease ended at 30, so only the renewals the hub admitted keep it live.
#[test]
fn a_rename_moves_the_addresses_a_peer_already_stored() {
    let (hub, _) = node("hub", START);
    let (spoke, _) = node("before", START);
    pair(&hub, &spoke);
    sync(&hub, &spoke);
    let agent_id = spoke.agent("worker").unwrap().agent_id;
    assert_eq!(hub.agent("worker@@before").unwrap().agent_id, agent_id);

    spoke.set_node_name("after").unwrap();
    sync(&hub, &spoke);
    let moved = hub.agent("worker@@after").unwrap();
    assert_eq!(moved.agent_id, agent_id);
    assert_eq!(moved.address, "worker@@after");
}

#[test]
fn an_owner_offline_past_its_own_expiry_hands_the_resource_to_the_borrower_the_hub_renewed() {
    let net = Net::new();
    let (id, rid) = net.resource("shared");
    net.renewed_to_65(&id);

    net.at(62);
    sync(&net.hub, &net.owner);
    net.owner.sweep().unwrap();

    let after = net.owner.resource(&id).unwrap();
    assert_eq!(after.status, "live", "not lapsed");
    assert_eq!(after.owner.as_deref(), Some(BORROWER));
    assert_eq!(after.refcount, 1);
    assert_eq!(net.owner.lapsed_for_ms(&rid).unwrap(), None);
    assert_eq!(kinds(&net.owner, "resource.transferred"), 1);

    sync(&net.hub, &net.owner);
    assert_eq!(
        net.hub.resource(&id).unwrap().owner.as_deref(),
        Some(BORROWER)
    );
}

/// Two agents on the borrower node, known to the hub, on a resource the owner holds to minute 60.
fn two_borrowers() -> (Net, ResourceRef, String) {
    let net = Net::new();
    for name in ["old", "new"] {
        net.borrower
            .register_agent_trusted(RegisterAgent {
                name: name.into(),
                program: Program::Codex,
                project: None,
                model: None,
                description: None,
            })
            .unwrap();
    }
    let (id, rid) = net.resource("shared");
    (net, id, rid)
}

#[test]
fn a_replayed_history_promotes_the_oldest_borrower_it_leaves_live() {
    let (net, id, rid) = two_borrowers();
    let borrow = |agent: &str, minutes: i64, ttl: i64| {
        net.at(minutes);
        net.borrower.borrow_resource(agent, &id, Some(ttl)).unwrap();
        sync(&net.hub, &net.borrower);
    };
    borrow("old", 0, 1800);
    borrow("new", 10, 7200);
    borrow("old", 20, 7200);

    net.at(61);
    net.owner.sweep().unwrap();
    assert!(net.owner.lapsed_for_ms(&rid).unwrap().is_some());

    // Old's first lease has ended by now, so only its renewal, later in this batch, keeps it live.
    net.at(100);
    pull(&net.hub, &net.owner);
    let after = net.owner.resource(&id).unwrap();
    assert_eq!(after.owner.as_deref(), Some("old@@borrower"));
    assert_eq!(after.refcount, 2);
    assert_eq!(kinds(&net.owner, "resource.transferred"), 1);
}

#[test]
fn a_history_split_across_pull_pages_is_settled_after_the_last_page() {
    let (net, id, rid) = two_borrowers();
    let borrow = |agent: &str, minutes: i64, ttl: i64| {
        net.at(minutes);
        net.borrower.borrow_resource(agent, &id, Some(ttl)).unwrap();
        sync(&net.hub, &net.borrower);
    };
    borrow("old", 0, 1800);
    borrow("new", 10, 7200);
    borrow("old", 20, 7200);

    net.at(61);
    net.owner.sweep().unwrap();
    net.at(100);
    let mut cursor = 0;
    loop {
        let page = net.hub.pull_for(net.owner.node_id(), cursor, 1).unwrap();
        net.owner
            .ingest_page(net.hub.node_id(), &page.events, &page.nodes, page.more)
            .unwrap();
        cursor = page.cursor;
        if page.more {
            assert!(net.owner.lapsed_for_ms(&rid).unwrap().is_some());
        } else {
            break;
        }
    }
    let after = net.owner.resource(&id).unwrap();
    assert_eq!(after.owner.as_deref(), Some("old@@borrower"));
    assert_eq!(kinds(&net.owner, "resource.transferred"), 1);
}

#[test]
fn a_replayed_borrower_that_released_in_the_same_history_is_not_promoted() {
    let (net, id, rid) = two_borrowers();
    net.borrower
        .borrow_resource("new", &id, Some(7200))
        .unwrap();
    sync(&net.hub, &net.borrower);
    net.at(5);
    net.borrower
        .borrow_resource("old", &id, Some(7200))
        .unwrap();
    sync(&net.hub, &net.borrower);
    net.at(10);
    net.borrower.release_resource("new", &id, false).unwrap();
    sync(&net.hub, &net.borrower);

    net.at(61);
    net.owner.sweep().unwrap();
    assert!(net.owner.lapsed_for_ms(&rid).unwrap().is_some());

    net.at(62);
    pull(&net.hub, &net.owner);
    let after = net.owner.resource(&id).unwrap();
    assert_eq!(after.owner.as_deref(), Some("old@@borrower"));
    assert_eq!(after.refcount, 1);
    assert_eq!(kinds(&net.owner, "resource.transferred"), 1);
}

#[test]
fn a_replayed_borrow_released_in_the_same_history_leaves_the_resource_lapsed() {
    let (net, id, rid) = two_borrowers();
    net.borrower
        .borrow_resource("old", &id, Some(7200))
        .unwrap();
    sync(&net.hub, &net.borrower);
    net.at(10);
    net.borrower.release_resource("old", &id, false).unwrap();
    sync(&net.hub, &net.borrower);

    net.at(61);
    net.owner.sweep().unwrap();
    let lapsed = net.owner.lapsed_for_ms(&rid).unwrap();
    assert!(lapsed.is_some());

    net.at(62);
    pull(&net.hub, &net.owner);
    assert_eq!(net.owner.resource(&id).unwrap().status, "lapsed");
    assert_eq!(kinds(&net.owner, "resource.transferred"), 0);
    assert!(net.owner.lapsed_for_ms(&rid).unwrap().is_some());
}

#[test]
fn a_late_observer_counts_a_lease_that_outlived_its_owner() {
    let (hub, hub_time) = node("hub", START);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let r = hub
        .register_resource(RegisterResource {
            agent: "worker".into(),
            kind: ResourceKind::ModelServer,
            name: "shared".into(),
            endpoint: Some(format!("http://{}", listener.local_addr().unwrap())),
            port: None,
            pid: None,
            project: None,
            purpose: None,
            mode: LeaseMode::Shared,
            ttl_seconds: Some(3600),
            metadata: None,
        })
        .unwrap()
        .resource;
    let id = ResourceRef::Id(r.resource_id);
    let (borrower, _) = node("borrower", START);
    pair(&hub, &borrower);
    sync(&hub, &borrower);
    borrower.borrow_resource("worker", &id, Some(7200)).unwrap();
    sync(&hub, &borrower);

    hub_time.store(START + 61 * MINUTE, Ordering::SeqCst);
    hub.sweep().unwrap();
    let (observer, _) = node("observer", START + 61 * MINUTE);
    pair(&hub, &observer);
    pull(&hub, &observer);

    let (at_owner, at_observer) = (hub.resource(&id).unwrap(), observer.resource(&id).unwrap());
    assert_eq!(at_owner.refcount, 1);
    assert_eq!(at_observer.refcount, at_owner.refcount);
    assert_eq!(at_observer.owner, at_owner.owner);
}

#[test]
fn a_borrow_delivered_after_local_lapse_reopens_the_resource_while_its_lease_is_live() {
    let net = Net::new();
    let (id, rid) = net.resource("shared");
    net.renewed_to_65(&id);

    net.at(61);
    net.owner.sweep().unwrap();
    assert_eq!(net.owner.resource(&id).unwrap().status, "lapsed");
    assert!(net.owner.lapsed_for_ms(&rid).unwrap().is_some());

    net.at(62);
    sync(&net.hub, &net.owner);
    let after = net.owner.resource(&id).unwrap();
    assert_eq!(after.status, "live");
    assert_eq!(after.owner.as_deref(), Some(BORROWER));
    assert_eq!(net.owner.lapsed_for_ms(&rid).unwrap(), None);
    assert_eq!(kinds(&net.owner, "resource.transferred"), 1);

    sync(&net.hub, &net.owner);
    assert_eq!(
        net.hub.resource(&id).unwrap().owner.as_deref(),
        Some(BORROWER)
    );
}

#[test]
fn a_borrow_delivered_after_lapse_and_after_its_own_expiry_changes_nothing() {
    let net = Net::new();
    let (id, rid) = net.resource("shared");
    net.borrow(&id); // Runs to 30 minutes.

    net.at(61);
    net.owner.sweep().unwrap();
    let lapsed_for = net.owner.lapsed_for_ms(&rid).unwrap().unwrap();

    net.at(70);
    sync(&net.hub, &net.owner);
    let after = net.owner.resource(&id).unwrap();
    assert_eq!(after.status, "lapsed");
    assert_eq!(after.refcount, 0);
    assert_eq!(after.owner, None);
    assert_eq!(
        net.owner.lapsed_for_ms(&rid).unwrap(),
        Some(lapsed_for + 9 * MINUTE),
        "the lapse clock keeps running from the same moment"
    );
    assert_eq!(kinds(&net.owner, "resource.transferred"), 0);
}

#[test]
fn a_stop_on_the_owner_outranks_a_delayed_borrow() {
    let net = Net::new();
    let (id, _) = net.resource("shared");
    net.renewed_to_65(&id);

    net.at(40);
    net.owner.release_resource("worker", &id, true).unwrap();
    net.at(41);
    sync(&net.hub, &net.owner);
    let at_owner = net.owner.resource(&id).unwrap();
    assert_eq!(at_owner.status, "released");
    assert_eq!(at_owner.refcount, 0);

    // The owner's push reached the hub in that sync, so the hub's copy ended too.
    let at_hub = net.hub.resource(&id).unwrap();
    assert_eq!(at_hub.status, "released");
    assert_eq!(at_hub.refcount, 0);
}

#[test]
fn a_renewal_that_reaches_the_hub_too_late_is_refused_and_not_relayed() {
    let net = Net::new();
    let (id, _) = net.resource("shared");
    net.borrow(&id); // Runs to 30 minutes.
    let (observer, observer_time) = node("observer", START);
    pair(&net.hub, &observer);
    pull(&net.hub, &observer);

    net.at(29);
    net.borrower
        .borrow_resource("worker", &id, Some(1800))
        .unwrap();
    net.at(31);
    let renewals = kinds(&net.hub, "resource.renewed");
    let report = push(&net.hub, &net.borrower);
    assert_eq!(report.rejected.len(), 1, "{report:?}");
    assert_eq!(kinds(&net.hub, "resource.renewed"), renewals);

    observer_time.store(START + 31 * MINUTE, Ordering::SeqCst);
    pull(&net.hub, &observer);
    assert_eq!(kinds(&observer, "resource.renewed"), renewals);
    assert_eq!(
        observer.resource(&id).unwrap().refcount,
        net.hub.resource(&id).unwrap().refcount,
    );
}

#[test]
fn one_stale_lease_does_not_cost_an_agent_its_healthy_renewals() {
    let net = Net::new();
    let (short, _) = net.resource("short");
    let (long, _) = net.resource("long");
    net.borrow(&short); // Runs to 30 minutes.
    net.borrower
        .borrow_resource("worker", &long, Some(3600))
        .unwrap();
    sync(&net.hub, &net.borrower);
    let hub_expiry = |id: &ResourceRef| {
        net.hub
            .resource(id)
            .unwrap()
            .leases
            .iter()
            .find(|l| l.agent == BORROWER)
            .map(|l| l.expires_at.clone())
    };
    let before = hub_expiry(&long);

    net.at(29);
    net.borrower.heartbeat_resources("worker", &[]).unwrap();
    net.at(31);
    let report = push(&net.hub, &net.borrower);
    assert_eq!(report.rejected.len(), 1, "{report:?}");

    assert_eq!(
        hub_expiry(&short),
        None,
        "expired at the hub, stays expired"
    );
    assert_ne!(hub_expiry(&long), before, "the healthy lease was renewed");
}
