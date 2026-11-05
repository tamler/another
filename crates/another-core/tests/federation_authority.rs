//! What a hub does with events a paired spoke pushes, checked with events that are properly
//! signed but say whatever the test wants. A signature proves who sent an event, not that the
//! sender had any say over what it describes. docs/FEDERATION.md: each node is authoritative
//! for its own agents, holds and resources, and events from other nodes "never rewrite another
//! node's ownership".
//!
//! BUGS-FOUND.md #11 regressions reject the signed forgeries before storing or relaying them.

use std::cell::Cell;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use another_core::*;
use serde_json::{json, Value};

const T0: i64 = 1_800_000_000_000;
const EVIL: &str = "EVIL-NODE";

fn hub() -> (Store, Arc<AtomicI64>) {
    let (clock, handle) = Clock::manual(T0);
    let store = Store::open_in_memory(clock).unwrap();
    store.set_node_name("hub").unwrap();
    store.ensure_project("t", "/work/shop").unwrap();
    for name in ["alice", "bob"] {
        store
            .register_agent_trusted(RegisterAgent {
                name: name.into(),
                program: Program::Claude,
                project: Some("/work/shop".into()),
                model: None,
                description: None,
            })
            .unwrap();
    }
    (store, handle)
}

/// A paired spoke holding its own key, signing whatever it is told to.
struct Spoke {
    key: ed25519_dalek::SigningKey,
    seq: Cell<u32>,
    node_id: String,
}

impl Spoke {
    fn paired_with(hub: &Store, perms: &[Perm]) -> Spoke {
        Self::paired_as(hub, perms, EVIL, "evil")
    }

    fn paired_as(hub: &Store, perms: &[Perm], node_id: &str, name: &str) -> Spoke {
        let key = crypto::generate_key().unwrap();
        let code = hub.create_invite("http://hub.test", perms, 3600).unwrap();
        let invite = decode(&code);
        hub.accept_pairing(&PairRequest {
            invite_id: invite.invite_id,
            secret: invite.secret,
            node: NodeEntry {
                node_id: node_id.into(),
                display_name: name.into(),
                public_key: crypto::public_of(&key),
            },
        })
        .unwrap();
        Spoke {
            key,
            seq: Cell::new(0),
            node_id: node_id.into(),
        }
    }

    fn event(&self, hub: &Store, kind: &str, payload: Value) -> WireEvent {
        self.seq.set(self.seq.get() + 1);
        let (event_id, payload, ts) = (
            format!("{}-EV-{}", self.node_id, self.seq.get()),
            payload.to_string(),
            hub.now(),
        );
        let msg = crypto::event_message(&event_id, &self.node_id, ts, "mallory", kind, &payload);
        WireEvent {
            seq: i64::from(self.seq.get()),
            event_id,
            node_id: self.node_id.clone(),
            ts,
            actor: "mallory".into(),
            kind: kind.into(),
            payload,
            prev_id: None,
            sig: crypto::sign(&self.key, &msg),
        }
    }

    /// Push one event and require the hub to accept it.
    fn accepted(&self, hub: &Store, kind: &str, payload: Value) {
        let ev = self.event(hub, kind, payload);
        let r = hub.ingest(&self.node_id, &[ev], &[]).unwrap();
        assert_eq!(r.accepted, 1, "{kind}: {:?}", r.rejected);
    }

    fn rejected(&self, hub: &Store, kind: &str, payload: Value) -> String {
        let ev = self.event(hub, kind, payload);
        let r = hub
            .ingest(&self.node_id, std::slice::from_ref(&ev), &[])
            .unwrap();
        assert_eq!(r.accepted, 0, "{kind} was accepted");
        assert_eq!(r.rejected.len(), 1, "{r:?}");
        assert!(hub
            .events(20_000)
            .unwrap()
            .iter()
            .all(|e| e.event_id != ev.event_id));
        r.rejected[0].reason.clone()
    }
}

fn decode(code: &str) -> Invite {
    let raw = code.strip_prefix("another-invite:").unwrap();
    serde_json::from_slice(&crypto::unb64(raw).unwrap()).unwrap()
}

fn agent_id(s: &Store, who: &str) -> String {
    s.agent(who).unwrap().agent_id
}

fn hold(s: &Store, agent: &str, path: &str) -> String {
    let r = s
        .acquire_holds(AcquireHolds {
            agent: agent.into(),
            project: "shop".into(),
            paths: vec![path.into()],
            ttl_seconds: Some(3600),
            exclusive: true,
            reason: None,
            lane: None,
        })
        .unwrap();
    r.granted[0].hold_id.clone()
}

fn resource(s: &Store, agent: &str, mode: LeaseMode) -> Resource {
    s.register_resource(RegisterResource {
        agent: agent.into(),
        kind: ResourceKind::ModelServer,
        name: "ollama@11434".into(),
        endpoint: None,
        port: None,
        pid: None,
        project: Some("shop".into()),
        purpose: None,
        mode,
        ttl_seconds: Some(3600),
        metadata: None,
    })
    .unwrap()
    .resource
}

fn send(s: &Store, from: &str, to: &str) -> Message {
    s.send_message(SendMessage {
        from: from.into(),
        to: vec![to.into()],
        subject: "hello".into(),
        body: "hi".into(),
        ..Default::default()
    })
    .unwrap()
}

/// A spoke's own agent and project on the hub.
fn spoke_agent(hub: &Store, spoke: &Spoke) -> String {
    spoke.accepted(
        hub,
        "project.ensured",
        json!({ "project_id": "EVIL-PROJECT", "abs_path": "/src/lab", "slug": "lab" }),
    );
    spoke.accepted(
        hub,
        "agent.registered",
        json!({ "agent_id": "EVIL-AGENT", "name": "eve", "project_id": "EVIL-PROJECT", "program": "codex" }),
    );
    "EVIL-AGENT".into()
}

fn receiver(hub: &Store, perms: &[Perm]) -> Store {
    receiver_at(hub, perms, T0)
}

fn receiver_at(hub: &Store, perms: &[Perm], now: i64) -> Store {
    let s = Store::open_in_memory(Clock::manual(now).0).unwrap();
    s.set_node_name("receiver").unwrap();
    let code = hub.create_invite("http://hub.test", perms, 86_400).unwrap();
    let (invite, req) = s.prepare_join(&code).unwrap();
    let response = hub.accept_pairing(&req).unwrap();
    s.complete_join(&invite, &response).unwrap();
    s
}

fn decision(s: &Store) -> Decision {
    s.ask_decision(
        NewDecision {
            agent: "alice@shop".into(),
            human: Some("reviewer".into()),
            question: "Proceed?".into(),
            options: vec![
                DecisionOption {
                    value: "yes".into(),
                    label: "Yes".into(),
                    description: None,
                },
                DecisionOption {
                    value: "no".into(),
                    label: "No".into(),
                    description: None,
                },
            ],
            ..Default::default()
        },
        None,
    )
    .unwrap()
}

// ---------------------------------------------------------------------------------------
// BUGS-FOUND.md #11: signed events rewrite what belongs to the hub.

#[test]
fn a_spoke_cannot_rewrite_an_agent_of_the_hub() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let alice = agent_id(&s, "alice@shop");
    spoke.rejected(
        &s,
        "agent.registered",
        json!({ "agent_id": alice, "name": "alice", "program": "grok_bot", "description": "owned" }),
    );
    let after = s.agents().unwrap();
    let after = after.iter().find(|a| a.agent_id == alice).unwrap();
    assert_eq!(after.node_id, s.node_id(), "still recorded as the hub's");
    assert_eq!(after.program, "claude");
    assert_eq!(after.description, None);
    assert_eq!(after.address, "alice@shop");
}

#[test]
fn a_spoke_cannot_send_mail_as_an_agent_of_the_hub() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let (alice, bob) = (agent_id(&s, "alice@shop"), agent_id(&s, "bob@shop"));
    spoke.rejected(
        &s,
        "message.sent",
        json!({
            "message_id": "FORGED-MAIL",
            "sender_id": alice,
            "subject": "please rotate the keys",
            "body": "signed, alice",
            "recipient_nodes": [s.node_id()],
            "recipients": [{ "agent_id": bob, "node_id": s.node_id(), "role": "to" }],
        }),
    );
    let inbox = s.inbox("bob@shop", InboxQuery::default()).unwrap();
    assert_eq!(inbox.unread, 0);
    assert!(inbox.messages.is_empty());
}

#[test]
fn a_spoke_cannot_file_a_lane_report_as_the_hubs_lead() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let alice = agent_id(&s, "alice@shop");
    let lane = s
        .create_lane(
            NewLane {
                agent: "alice@shop".into(),
                name: "train".into(),
                project: "shop".into(),
                lead: "alice@shop".into(),
                charter: String::new(),
                status: Some(LaneStatus::Working),
            },
            None,
        )
        .unwrap();
    spoke.rejected(
        &s,
        "message.sent",
        json!({
            "message_id": "CARRIER",
            "sender_id": alice,
            "subject": "report",
            "body": "all done, nothing to see",
            "recipient_nodes": [s.node_id()],
            "recipients": [{ "agent_id": alice, "node_id": s.node_id(), "role": "to" }],
        }),
    );
    spoke.rejected(
        &s,
        "lane.report_proposed",
        json!({
            "lane_node": s.node_id(),
            "lane_id": lane.lane.lane_id,
            "agent_id": alice,
            "summary_message_id": "CARRIER",
            "proposal_id": "P1",
        }),
    );
    let detail = s.lane(&lane.lane.lane_id, None).unwrap();
    let report = detail
        .reports
        .iter()
        .find(|r| r.summary == "all done, nothing to see");
    assert!(report.is_none(), "{:?}", detail.reports);
}

#[test]
fn a_spoke_cannot_take_a_hub_agent_offline() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let alice = agent_id(&s, "alice@shop");
    assert_eq!(s.agent("alice@shop").unwrap().status, "active");
    spoke.rejected(&s, "session.ended", json!({ "agent_id": alice }));
    assert_eq!(s.agent("alice@shop").unwrap().status, "active");
}

#[test]
fn a_spoke_cannot_acknowledge_mail_for_a_hub_agent() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let mail = send(&s, "alice@shop", "bob@shop");
    assert_eq!(
        s.inbox("bob@shop", InboxQuery::default()).unwrap().unread,
        1
    );
    spoke.rejected(
        &s,
        "delivery.acked",
        json!({
            "message_id": mail.message_id,
            "agent_id": agent_id(&s, "bob@shop"),
            "message_node": s.node_id(),
        }),
    );
    assert_eq!(
        s.inbox("bob@shop", InboxQuery::default()).unwrap().unread,
        1,
        "bob never read it"
    );
}

#[test]
fn a_spoke_cannot_stop_leases_on_a_hub_resource() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let r = resource(&s, "alice@shop", LeaseMode::Shared);
    s.borrow_resource("bob@shop", &ResourceRef::Id(r.resource_id.clone()), None)
        .unwrap();
    spoke.rejected(
        &s,
        "resource.stopped",
        json!({ "resource_id": r.resource_id }),
    );
    let after = s.resource(&ResourceRef::Id(r.resource_id.clone())).unwrap();
    // The resource itself is protected: only its node's word ends it.
    assert_ne!(after.status, "released");
    assert_eq!(after.leases.len(), 2, "{:?}", after.leases);
}

#[test]
fn a_spoke_cannot_promote_a_borrower_on_a_hub_resource() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let r = resource(&s, "alice@shop", LeaseMode::Shared);
    let lease = s
        .borrow_resource("bob@shop", &ResourceRef::Id(r.resource_id.clone()), None)
        .unwrap()
        .lease
        .unwrap();
    assert_eq!(lease.role, "borrower");
    spoke.rejected(
        &s,
        "resource.transferred",
        json!({ "lease_id": lease.lease_id }),
    );
    let after = s.resource(&ResourceRef::Id(r.resource_id)).unwrap();
    assert!(after
        .leases
        .iter()
        .any(|l| l.lease_id == lease.lease_id && l.role == "borrower"));
}

#[test]
fn a_spoke_cannot_end_a_hub_hold_by_rewriting_its_expiry() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let id = hold(&s, "alice@shop", "src/main.rs");
    spoke.rejected(
        &s,
        "hold.updated",
        json!({
            "hold_id": id,
            "project_id": s.ensure_project("t", "/work/shop").unwrap().project_id,
            "agent_id": agent_id(&s, "alice@shop"),
            "pattern": "src/main.rs",
            "expires_at": 1,
        }),
    );
    assert_eq!(s.holds(HoldFilter::default()).unwrap().len(), 1);
}

/// The permission check reads `resource_node` from the payload, which the spoke writes.
#[test]
fn a_spoke_cannot_borrow_without_permission_by_omitting_the_node() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &[]);
    let eve = spoke_agent(&s, &spoke);
    let r = resource(&s, "alice@shop", LeaseMode::Shared);
    let reason = spoke.rejected(
        &s,
        "resource.borrowed",
        json!({ "resource_id": r.resource_id, "resource_node": s.node_id(), "lease_id": "L1", "agent_id": eve }),
    );
    assert!(reason.contains("may not borrow"), "{reason}");
    spoke.rejected(
        &s,
        "resource.borrowed",
        json!({ "resource_id": r.resource_id, "lease_id": "L2", "agent_id": eve }),
    );
    let after = s.resource(&ResourceRef::Id(r.resource_id)).unwrap();
    assert_eq!(
        after.refcount, 1,
        "the spoke has no borrow permission: {:?}",
        after.leases
    );
}

/// `resource.adopted` needs no permission and ignores the resource's mode and node, and
/// `resource.registered` reaches the same code for an id the hub already has.
#[test]
fn a_spoke_cannot_take_over_a_lapsed_hub_resource() {
    for (kind, mode) in [
        ("resource.adopted", LeaseMode::Exclusive),
        ("resource.registered", LeaseMode::Shared),
    ] {
        let (s, _) = hub();
        let spoke = Spoke::paired_with(&s, &[]);
        let eve = spoke_agent(&s, &spoke);
        let r = resource(&s, "alice@shop", mode);
        let id = ResourceRef::Id(r.resource_id.clone());
        s.release_resource("alice@shop", &id, false).unwrap();
        assert_eq!(s.resource(&id).unwrap().status, "lapsed");
        assert!(s.lapsed_for_ms(&r.resource_id).unwrap().is_some());

        let (lease, owner) = if kind == "resource.adopted" {
            ("lease_id", "agent_id")
        } else {
            ("lease_id", "owner_id")
        };
        spoke.rejected(
            &s,
            kind,
            json!({ "resource_id": r.resource_id, lease: "TAKEN", owner: eve }),
        );
        let after = s.resource(&id).unwrap();
        assert_eq!(
            (after.status.as_str(), after.owner.as_deref()),
            ("lapsed", None),
            "{kind}: a spoke cannot own this resource"
        );
        assert_eq!(
            s.lapsed_for_ms(&r.resource_id).unwrap(),
            Some(0),
            "{kind}: the lapse clock is preserved"
        );
    }
}

#[test]
fn a_spoke_cannot_rename_itself_after_the_hub_or_use_an_invalid_name() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    for name in ["hub", "no spaces/allowed here"] {
        spoke.rejected(&s, "node.renamed", json!({ "display_name": name }));
        let peers = s.peers().unwrap();
        assert_eq!(peers[0].display_name, "evil", "pairing refuses {name} too");
    }
}

#[test]
fn both_variants_of_each_authority_violation_are_refused() {
    for kind in [
        "session.started",
        "delivery.read",
        "hold.acquired",
        "resource.reaped",
    ] {
        let (s, _) = hub();
        let spoke = Spoke::paired_with(&s, &Perm::ALL);
        let alice = agent_id(&s, "alice@shop");
        let bob = agent_id(&s, "bob@shop");
        let mail = send(&s, "alice@shop", "bob@shop");
        let hid = hold(&s, "alice@shop", "a.rs");
        let r = resource(&s, "alice@shop", LeaseMode::Shared);
        if kind == "session.started" {
            s.heartbeat("alice@shop", true).unwrap();
        }
        let before = s.agent("alice@shop").unwrap().status;
        let project = s.ensure_project("t", "/work/shop").unwrap().project_id;
        let payload = match kind {
            "session.started" => json!({ "agent_id": alice }),
            "delivery.read" => {
                json!({ "agent_id": bob, "message_id": mail.message_id, "message_node": s.node_id() })
            }
            "hold.acquired" => {
                json!({ "hold_id": hid, "agent_id": alice, "project_id": project, "pattern": "a.rs", "expires_at": 1 })
            }
            _ => json!({ "resource_id": r.resource_id }),
        };
        spoke.rejected(&s, kind, payload);
        assert_eq!(s.agent("alice@shop").unwrap().status, before);
        assert_eq!(
            s.inbox("bob@shop", InboxQuery::default()).unwrap().unread,
            1
        );
        if kind != "session.started" {
            assert_eq!(s.holds(HoldFilter::default()).unwrap().len(), 1);
            assert_eq!(
                s.resource(&ResourceRef::Id(r.resource_id))
                    .unwrap()
                    .refcount,
                1
            );
        }
    }
}

#[test]
fn a_spoke_cannot_collide_with_another_spokes_objects_or_borrow_its_identity() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let eve = spoke_agent(&s, &spoke);
    let other = Spoke::paired_as(&s, &Perm::ALL, "THIRD", "third");
    other.accepted(
        &s,
        "project.ensured",
        json!({ "project_id": "P3", "abs_path": "/work/third", "slug": "third" }),
    );
    other.accepted(
        &s,
        "agent.registered",
        json!({ "agent_id": "A3", "name": "worker", "project_id": "P3", "program": "claude" }),
    );
    other.accepted(
        &s,
        "hold.acquired",
        json!({ "hold_id": "H3", "project_id": "P3", "agent_id": "A3", "pattern": "a.rs" }),
    );
    other.accepted(&s, "resource.registered", json!({ "resource_id": "R3", "kind": "model_server", "name": "shared", "lease_id": "L3", "owner_id": "A3" }));
    let local = resource(&s, "alice@shop", LeaseMode::Shared);
    for (kind, payload) in [
        (
            "project.ensured",
            json!({ "project_id": "P3", "abs_path": "/changed", "slug": "changed" }),
        ),
        (
            "agent.registered",
            json!({ "agent_id": "A3", "name": "worker", "program": "grok_bot" }),
        ),
        (
            "agent.registered",
            json!({ "agent_id": "BAD-A", "name": "impostor", "project_id": "P3" }),
        ),
        (
            "hold.updated",
            json!({ "hold_id": "H3", "project_id": "EVIL-PROJECT", "agent_id": eve, "expires_at": 1 }),
        ),
        (
            "hold.acquired",
            json!({ "hold_id": "BAD-H", "project_id": "EVIL-PROJECT", "agent_id": "A3", "pattern": "a.rs" }),
        ),
        (
            "resource.registered",
            json!({ "resource_id": "R3", "lease_id": "BAD-L", "owner_id": eve }),
        ),
        (
            "resource.adopted",
            json!({ "resource_id": "R3", "lease_id": "BAD-L", "agent_id": eve }),
        ),
        (
            "resource.borrowed",
            json!({ "resource_id": local.resource_id, "resource_node": s.node_id(), "lease_id": "BAD-L", "agent_id": "A3" }),
        ),
        (
            "lane.report_proposed",
            json!({ "lane_node": "THIRD", "lane_id": "REMOTE-LANE", "agent_id": "A3", "summary_message_id": "CARRIER" }),
        ),
    ] {
        spoke.rejected(&s, kind, payload);
    }
    assert_eq!(s.agent("worker@third@third").unwrap().program, "claude");
    assert_eq!(
        s.resource(&ResourceRef::Id("R3".into()))
            .unwrap()
            .owner
            .as_deref(),
        Some("worker@third@third")
    );
    assert_eq!(
        s.resource(&ResourceRef::Id(local.resource_id))
            .unwrap()
            .refcount,
        1
    );
}

#[test]
fn a_message_id_collision_cannot_add_recipients_to_existing_mail() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let eve = spoke_agent(&s, &spoke);
    let mail = send(&s, "alice@shop", "bob@shop");
    spoke.rejected(
        &s,
        "message.sent",
        json!({
            "message_id": mail.message_id, "sender_id": eve,
            "recipient_nodes": [s.node_id()],
            "recipients": [{ "agent_id": eve, "node_id": EVIL, "role": "to" }],
        }),
    );
    assert_eq!(s.message(&mail.message_id).unwrap().to, vec!["bob@shop"]);
}

#[test]
fn transfer_authority_comes_from_the_stored_lease_not_the_claimed_resource() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let eve = spoke_agent(&s, &spoke);
    spoke.accepted(&s, "resource.registered", json!({ "resource_id": "OWN-R", "kind": "model_server", "name": "own", "owner_id": eve, "lease_id": "OWN-L" }));
    let r = resource(&s, "alice@shop", LeaseMode::Shared);
    let lease = s
        .borrow_resource("bob@shop", &ResourceRef::Id(r.resource_id.clone()), None)
        .unwrap()
        .lease
        .unwrap();
    spoke.rejected(
        &s,
        "resource.transferred",
        json!({ "resource_id": "OWN-R", "lease_id": lease.lease_id }),
    );
    assert!(s
        .resource(&ResourceRef::Id(r.resource_id))
        .unwrap()
        .leases
        .iter()
        .any(|l| l.lease_id == lease.lease_id && l.role == "borrower"));
}

#[test]
fn a_permitted_borrower_cannot_revive_a_lapsed_resource_or_reset_its_clock() {
    let (s, clock) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let eve = spoke_agent(&s, &spoke);
    let r = resource(&s, "alice@shop", LeaseMode::Shared);
    s.release_resource("alice@shop", &ResourceRef::Id(r.resource_id.clone()), false)
        .unwrap();
    clock.fetch_add(120_000, Ordering::SeqCst);
    let before = s.lapsed_for_ms(&r.resource_id).unwrap();
    let mut ev = spoke.event(&s, "resource.borrowed", json!({ "resource_id": r.resource_id, "resource_node": s.node_id(), "agent_id": eve, "lease_id": "BACKDATED", "ttl_seconds": 3600 }));
    ev.ts = T0;
    ev.sig = crypto::sign(
        &spoke.key,
        &crypto::event_message(
            &ev.event_id,
            &ev.node_id,
            ev.ts,
            &ev.actor,
            &ev.kind,
            &ev.payload,
        ),
    );
    let event_id = ev.event_id.clone();
    let result = s.ingest(EVIL, &[ev], &[]).unwrap();
    assert_eq!(result.accepted, 0, "{result:?}");
    assert_eq!(result.rejected.len(), 1, "{result:?}");
    assert!(s
        .events(20_000)
        .unwrap()
        .iter()
        .all(|e| e.event_id != event_id));
    assert_eq!(
        s.resource(&ResourceRef::Id(r.resource_id.clone()))
            .unwrap()
            .refcount,
        0
    );
    assert_eq!(s.lapsed_for_ms(&r.resource_id).unwrap(), before);
}

#[test]
fn an_expired_foreign_borrower_lease_cannot_be_revived_by_renewal() {
    let (s, clock) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let eve = spoke_agent(&s, &spoke);
    let r = resource(&s, "alice@shop", LeaseMode::Shared);
    let id = ResourceRef::Id(r.resource_id.clone());
    spoke.accepted(&s, "resource.borrowed", json!({ "resource_id": r.resource_id, "resource_node": s.node_id(), "lease_id": "SHORT", "agent_id": eve, "ttl_seconds": 60 }));
    clock.fetch_add(120_000, Ordering::SeqCst);
    s.release_resource("alice@shop", &id, false).unwrap();
    let before = s.lapsed_for_ms(&r.resource_id).unwrap();
    spoke.rejected(
        &s,
        "resource.renewed",
        json!({ "lease_ids": ["SHORT"], "ttl_seconds": 3600 }),
    );
    assert_eq!(s.resource(&id).unwrap().refcount, 0);
    assert_eq!(s.lapsed_for_ms(&r.resource_id).unwrap(), before);
}

#[test]
fn a_spoke_cannot_claim_a_foreign_decision_asker_project_or_lane() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let eve = spoke_agent(&s, &spoke);
    let alice = agent_id(&s, "alice@shop");
    let project = s.ensure_project("t", "/work/shop").unwrap().project_id;
    let d = decision(&s);
    let lane = s
        .create_lane(
            NewLane {
                agent: "alice@shop".into(),
                name: "owned".into(),
                project: "shop".into(),
                lead: "alice@shop".into(),
                charter: String::new(),
                status: None,
            },
            None,
        )
        .unwrap();
    for payload in [
        json!({ "decision_id": "BAD-ASKER", "asker_id": alice, "project_id": "EVIL-PROJECT" }),
        json!({ "decision_id": "BAD-PROJECT", "asker_id": eve, "project_id": project }),
        json!({ "decision_id": d.decision_id, "asker_id": eve, "project_id": "EVIL-PROJECT" }),
        json!({ "decision_id": "BAD-LANE", "asker_id": eve, "project_id": "EVIL-PROJECT", "lane_id": lane.lane.lane_id }),
    ] {
        spoke.rejected(&s, "decision.created", payload);
    }
    for kind in [
        "decision.answered",
        "decision.expired",
        "decision.cancelled",
    ] {
        spoke.accepted(
            &s,
            kind,
            json!({ "decision_id": d.decision_id, "value": "yes" }),
        );
        assert_eq!(
            s.decision(&d.decision_id, None).unwrap().status,
            DecisionStatus::Open
        );
    }
}

#[test]
fn decision_mirrors_do_not_copy_another_nodes_mail_body() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let eve = spoke_agent(&s, &spoke);
    let mail = send(&s, "alice@shop", "bob@shop");
    spoke.accepted(&s, "decision.created", json!({ "decision_id": "MIRROR", "asker_id": eve, "project_id": "EVIL-PROJECT", "message_id": mail.message_id, "human": "reviewer", "option_values": ["yes", "no"] }));
    let d = s.decision("MIRROR", None).unwrap();
    assert_ne!(d.question, mail.body);
    assert!(
        d.question.starts_with("(question asked on node"),
        "{}",
        d.question
    );
}

fn check_foreign_carrier(kind: &str) {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let eve = spoke_agent(&s, &spoke);
    let d = decision(&s);
    let lane = s
        .create_lane(
            NewLane {
                agent: "alice@shop".into(),
                name: "train".into(),
                project: "shop".into(),
                lead: "alice@shop".into(),
                charter: String::new(),
                status: Some(LaneStatus::Working),
            },
            None,
        )
        .unwrap();
    let mail = send(&s, "alice@shop", "bob@shop");
    if kind == "lane.report_proposed" {
        spoke.accepted(&s, kind, json!({ "lane_node": s.node_id(), "lane_id": lane.lane.lane_id, "agent_id": eve, "summary_message_id": mail.message_id, "proposal_id": "P" }));
    } else {
        spoke.accepted(&s, kind, json!({ "decision_node": s.node_id(), "decision_id": d.decision_id, "relayed_by_id": eve, "text_message_id": mail.message_id, "value": "not-an-option" }));
    }
    assert_eq!(
        s.inbox("bob@shop", InboxQuery::default()).unwrap().unread,
        1
    );
    assert_eq!(
        s.decision(&d.decision_id, None).unwrap().status,
        DecisionStatus::Open
    );
}

#[test]
fn a_proposal_cannot_mark_a_hub_carrier_read() {
    check_foreign_carrier("lane.report_proposed");
}

#[test]
fn an_answer_relay_cannot_mark_a_hub_carrier_read() {
    check_foreign_carrier("decision.answer_relayed");
}

#[test]
fn a_feed_only_receiver_projects_a_permitted_borrow_and_owner_promotion() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let eve = spoke_agent(&s, &spoke);
    let observer = receiver(&s, &[Perm::ReadFeed]);
    let r = resource(&s, "alice@shop", LeaseMode::Shared);
    let id = ResourceRef::Id(r.resource_id.clone());
    spoke.accepted(&s, "resource.borrowed", json!({ "resource_id": r.resource_id, "resource_node": s.node_id(), "lease_id": "BORROW", "agent_id": eve }));
    let page = s.pull_for(observer.node_id(), 0, 2000).unwrap();
    let applied = observer
        .ingest(s.node_id(), &page.events, &page.nodes)
        .unwrap();
    assert!(applied.rejected.is_empty(), "{applied:?}");
    assert_eq!(observer.resource(&id).unwrap().refcount, 2);
    s.release_resource("alice@shop", &id, false).unwrap();
    assert_eq!(
        s.resource(&id).unwrap().owner.as_deref(),
        Some("eve@lab@evil")
    );
    let next = s.pull_for(observer.node_id(), page.cursor, 2000).unwrap();
    let applied = observer
        .ingest(s.node_id(), &next.events, &next.nodes)
        .unwrap();
    assert!(applied.rejected.is_empty(), "{applied:?}");
    assert_eq!(
        observer.resource(&id).unwrap().owner.as_deref(),
        Some("eve@lab@evil")
    );
}

#[test]
fn duplicate_retries_are_not_revalidated_against_new_name_ownership() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let ev = spoke.event(&s, "node.renamed", json!({ "display_name": "old-name" }));
    assert_eq!(
        s.ingest(EVIL, std::slice::from_ref(&ev), &[])
            .unwrap()
            .accepted,
        1
    );
    spoke.accepted(&s, "node.renamed", json!({ "display_name": "new-name" }));
    s.set_node_name("old-name").unwrap();
    let result = s.ingest(EVIL, &[ev], &[]).unwrap();
    assert_eq!(
        (result.accepted, result.duplicates, result.rejected.len()),
        (0, 1, 0)
    );
    assert_eq!(s.peers().unwrap()[0].display_name, "new-name");
}

#[test]
fn agent_updates_cannot_forge_addresses_or_erase_a_missing_name() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let eve = spoke_agent(&s, &spoke);
    spoke.rejected(
        &s,
        "agent.registered",
        json!({ "agent_id": eve, "name": "alice@shop@hub", "project_id": "EVIL-PROJECT" }),
    );
    spoke.rejected(
        &s,
        "project.ensured",
        json!({ "project_id": "BAD-P", "abs_path": "/work/forged", "slug": "shop@hub" }),
    );
    spoke.accepted(&s, "agent.registered", json!({ "agent_id": eve }));
    assert_eq!(s.agent("eve@lab@evil").unwrap().address, "eve@lab@evil");
}

#[test]
fn a_spokes_system_agent_can_send_real_signed_mail() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    spoke.accepted(
        &s,
        "agent.registered",
        json!({ "agent_id": "SYSTEM", "name": "another", "program": "other" }),
    );
    let bob = agent_id(&s, "bob@shop");
    spoke.accepted(&s, "message.sent", json!({ "message_id": "SYSTEM-MAIL", "sender_id": "SYSTEM", "subject": "summary", "body": "ready", "recipient_nodes": [s.node_id()], "recipients": [{ "agent_id": bob, "node_id": EVIL, "role": "to" }] }));
    let inbox = s.inbox("bob@shop", InboxQuery::default()).unwrap();
    assert_eq!(inbox.messages[0].message.from, "another@@evil");
    s.acknowledge("bob@shop", &["SYSTEM-MAIL".into()], true)
        .unwrap();
    let (out, _) = s.outbound_events(0, 2000).unwrap();
    let ack = out.iter().find(|e| e.kind == "delivery.acked").unwrap();
    assert_eq!(ack.payload.parse::<Value>().unwrap()["message_node"], EVIL);
}

#[test]
fn a_hub_relay_does_not_grant_authority_over_the_receivers_agents() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    spoke_agent(&s, &spoke);
    let observer = receiver(&s, &[Perm::ReadFeed]);
    let page = s.pull_for(observer.node_id(), 0, 2000).unwrap();
    observer
        .ingest(s.node_id(), &page.events, &page.nodes)
        .unwrap();
    let alice = observer.agent("alice@shop@hub").unwrap();
    let ev = spoke.event(
        &s,
        "agent.registered",
        json!({ "agent_id": alice.agent_id, "name": "alice", "program": "grok_bot" }),
    );
    let result = observer
        .ingest(s.node_id(), std::slice::from_ref(&ev), &[])
        .unwrap();
    assert_eq!((result.accepted, result.rejected.len()), (0, 1));
    assert_eq!(observer.agent("alice@shop@hub").unwrap().program, "claude");
    assert!(observer
        .events(20_000)
        .unwrap()
        .iter()
        .all(|e| e.event_id != ev.event_id));
}

#[test]
fn complete_unknown_borrows_are_refused_on_push_and_noop_on_relay() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let eve = spoke_agent(&s, &spoke);
    let observer = receiver(&s, &[Perm::ReadFeed]);
    let page = s.pull_for(observer.node_id(), 0, 2000).unwrap();
    observer
        .ingest(s.node_id(), &page.events, &page.nodes)
        .unwrap();
    let ev = spoke.event(&s, "resource.borrowed", json!({ "resource_id": "UNKNOWN", "resource_node": s.node_id(), "agent_id": eve, "lease_id": "UNKNOWN-L" }));
    let result = s.ingest(EVIL, std::slice::from_ref(&ev), &[]).unwrap();
    assert_eq!((result.accepted, result.rejected.len()), (0, 1));
    let result = observer.ingest(s.node_id(), &[ev], &[]).unwrap();
    assert_eq!((result.accepted, result.rejected.len()), (1, 0));
    assert!(observer
        .resource(&ResourceRef::Id("UNKNOWN".into()))
        .is_err());
}

#[test]
fn borrowing_permission_and_routing_cannot_be_forged_in_payloads() {
    for perms in [&[][..], &Perm::ALL[..]] {
        let (s, _) = hub();
        let spoke = Spoke::paired_with(&s, perms);
        let eve = spoke_agent(&s, &spoke);
        let r = resource(&s, "alice@shop", LeaseMode::Shared);
        for hint in [Value::Null, json!(EVIL), json!("WRONG-NODE")] {
            spoke.rejected(&s, "resource.borrowed", json!({ "resource_id": r.resource_id, "resource_node": hint, "lease_id": format!("L-{}", spoke.seq.get()), "agent_id": eve }));
        }
        assert_eq!(
            s.resource(&ResourceRef::Id(r.resource_id))
                .unwrap()
                .refcount,
            1
        );
    }
}

#[test]
fn lane_creators_and_updated_leads_cannot_be_foreign_acting_identities() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let eve = spoke_agent(&s, &spoke);
    let alice = agent_id(&s, "alice@shop");
    spoke.rejected(&s, "lane.created", json!({ "lane_id": "BAD-CREATOR", "project_id": "EVIL-PROJECT", "name": "forged", "lead_id": eve, "created_by": alice }));
    spoke.rejected(&s, "lane.created", json!({ "lane_id": "BAD-LEAD", "project_id": "EVIL-PROJECT", "name": "forged", "lead_id": alice }));
    spoke.accepted(&s, "lane.created", json!({ "lane_id": "OWN-LANE", "project_id": "EVIL-PROJECT", "name": "own", "lead_id": eve }));
    spoke.rejected(
        &s,
        "lane.updated",
        json!({ "lane_id": "OWN-LANE", "lead_id": alice }),
    );
    assert_eq!(s.lane("OWN-LANE", None).unwrap().lane.lead, "eve@lab@evil");
    // The actor display string cannot supply the missing creator from the hub's namespace.
    let mut ev = spoke.event(&s, "lane.created", json!({ "lane_id": "ACTOR-LANE", "project_id": "EVIL-PROJECT", "name": "actor", "lead_id": eve }));
    ev.actor = "alice@shop".into();
    ev.sig = crypto::sign(
        &spoke.key,
        &crypto::event_message(
            &ev.event_id,
            &ev.node_id,
            ev.ts,
            &ev.actor,
            &ev.kind,
            &ev.payload,
        ),
    );
    assert_eq!(s.ingest(EVIL, &[ev], &[]).unwrap().accepted, 1);
    assert_eq!(
        s.lane("ACTOR-LANE", None).unwrap().lane.created_by,
        "eve@lab@evil"
    );
}

#[test]
fn another_node_cannot_collide_with_or_close_a_decision_mirror() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let eve = spoke_agent(&s, &spoke);
    spoke.accepted(&s, "decision.created", json!({ "decision_id": "REMOTE-D", "asker_id": eve, "project_id": "EVIL-PROJECT", "human": "reviewer", "option_values": ["yes", "no"] }));
    let other = Spoke::paired_as(&s, &Perm::ALL, "THIRD", "third");
    other.rejected(&s, "decision.created", json!({ "decision_id": "REMOTE-D" }));
    for kind in [
        "decision.answered",
        "decision.expired",
        "decision.cancelled",
    ] {
        other.accepted(
            &s,
            kind,
            json!({ "decision_id": "REMOTE-D", "value": "yes" }),
        );
        assert_eq!(
            s.decision("REMOTE-D", None).unwrap().status,
            DecisionStatus::Open
        );
    }
}

#[test]
fn lease_id_collisions_cannot_reset_a_lapsed_resource() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let eve = spoke_agent(&s, &spoke);
    let r = resource(&s, "alice@shop", LeaseMode::Shared);
    let lease = &r.leases[0].lease_id;
    spoke.rejected(&s, "resource.registered", json!({ "resource_id": "OWN-R", "kind": "model_server", "name": "own", "owner_id": eve, "lease_id": lease }));
    assert!(s.resource(&ResourceRef::Id("OWN-R".into())).is_err());
    assert_eq!(
        s.resource(&ResourceRef::Id(r.resource_id))
            .unwrap()
            .refcount,
        1
    );
}

#[test]
fn late_joiner_replays_an_admitted_borrow_that_outlived_its_owner_lease() {
    let (s, clock) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let eve = spoke_agent(&s, &spoke);
    let r = resource(&s, "alice@shop", LeaseMode::Shared);
    spoke.accepted(&s, "resource.borrowed", json!({ "resource_id": r.resource_id, "resource_node": s.node_id(), "agent_id": eve, "lease_id": "HISTORY", "ttl_seconds": 7200 }));
    clock.store(T0 + 3_601_000, Ordering::SeqCst);
    s.sweep().unwrap();
    let observer = receiver_at(&s, &[Perm::ReadFeed], T0 + 3_601_000);
    let page = s.pull_for(observer.node_id(), 0, 2000).unwrap();
    let result = observer
        .ingest(s.node_id(), &page.events, &page.nodes)
        .unwrap();
    assert!(result.rejected.is_empty(), "{result:?}");
    let id = ResourceRef::Id(r.resource_id);
    let (at_hub, at_observer) = (s.resource(&id).unwrap(), observer.resource(&id).unwrap());
    assert_eq!(at_hub.refcount, 1);
    assert_eq!(at_observer.refcount, at_hub.refcount);
    assert_eq!(at_observer.owner, at_hub.owner);
}

#[test]
fn a_renewal_naming_an_expired_lease_is_refused_whole_and_never_reaches_a_mirror() {
    let (s, clock) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let eve = spoke_agent(&s, &spoke);
    let r = resource(&s, "alice@shop", LeaseMode::Shared);
    let id = ResourceRef::Id(r.resource_id.clone());
    spoke.accepted(&s, "resource.borrowed", json!({ "resource_id": r.resource_id, "resource_node": s.node_id(), "agent_id": eve, "lease_id": "LIVE", "ttl_seconds": 7200 }));
    spoke.accepted(&s, "resource.borrowed", json!({ "resource_id": r.resource_id, "resource_node": s.node_id(), "agent_id": eve, "lease_id": "SHORT", "ttl_seconds": 60 }));
    clock.fetch_add(120_000, Ordering::SeqCst);

    // A renewal naming an expired lease is refused whole: stored, it would replay on mirrors.
    let reason = spoke.rejected(
        &s,
        "resource.renewed",
        json!({ "lease_ids": ["LIVE", "SHORT"], "ttl_seconds": 7200 }),
    );
    assert!(reason.contains("expired"), "{reason}");

    let observer = receiver_at(&s, &[Perm::ReadFeed], T0 + 120_000);
    let page = s.pull_for(observer.node_id(), 0, 2000).unwrap();
    let result = observer
        .ingest(s.node_id(), &page.events, &page.nodes)
        .unwrap();
    assert!(result.rejected.is_empty(), "{result:?}");
    let (at_hub, at_observer) = (s.resource(&id).unwrap(), observer.resource(&id).unwrap());
    assert_eq!(at_hub.refcount, 2, "the owner and the live borrower");
    assert_eq!(at_observer.refcount, at_hub.refcount);
}

#[test]
fn a_late_joiner_skips_conflicting_rename_history_and_keeps_syncing() {
    let (s, _) = hub();
    let first = Spoke::paired_as(&s, &Perm::ALL, "FIRST", "alpha");
    let second = Spoke::paired_as(&s, &Perm::ALL, "SECOND", "beta");
    first.accepted(&s, "node.renamed", json!({ "display_name": "alpha" }));
    first.accepted(&s, "node.renamed", json!({ "display_name": "gamma" }));
    second.accepted(&s, "node.renamed", json!({ "display_name": "beta" }));
    second.accepted(&s, "node.renamed", json!({ "display_name": "alpha" }));
    let observer = receiver(&s, &[Perm::ReadFeed]);
    let page = s.pull_for(observer.node_id(), 0, 2000).unwrap();
    let report = observer
        .ingest(s.node_id(), &page.events, &page.nodes)
        .unwrap();
    assert_eq!(report.rejected.len(), 1, "{report:?}");
    assert!(report.rejected[0].reason.contains("already named"));
    assert!(report.accepted > 0);
    // The next directory snapshot is still usable, including valid simultaneous name swaps.
    let mut nodes = page.nodes;
    for node in &mut nodes {
        if node.node_id == "FIRST" {
            node.display_name = "alpha".into();
        }
        if node.node_id == "SECOND" {
            node.display_name = "gamma".into();
        }
    }
    assert!(observer
        .ingest(s.node_id(), &[], &nodes)
        .unwrap()
        .rejected
        .is_empty());
    assert!(matches!(
        observer.set_node_name("alpha"),
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        observer.set_node_name("gamma"),
        Err(Error::Conflict(_))
    ));
    assert!(observer.set_node_name("unused").is_ok());
}

// ---------------------------------------------------------------------------------------
// Where the rules hold: a spoke's word doesn't end what belongs to the hub.

#[test]
fn a_spoke_cannot_end_the_hubs_holds_leases_lanes_or_resources() {
    let (s, clock) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let hold_id = hold(&s, "alice@shop", "src/main.rs");
    let hold_expiry = s.holds(HoldFilter::default()).unwrap()[0]
        .expires_at
        .clone();
    clock.fetch_add(60_000, Ordering::SeqCst);
    let r = resource(&s, "alice@shop", LeaseMode::Shared);
    let lease = r.leases[0].lease_id.clone();
    let lane = s
        .create_lane(
            NewLane {
                agent: "alice@shop".into(),
                name: "train".into(),
                project: "shop".into(),
                lead: "alice@shop".into(),
                charter: String::new(),
                status: Some(LaneStatus::Working),
            },
            None,
        )
        .unwrap()
        .lane
        .lane_id;
    let bob = agent_id(&s, "bob@shop");
    for (kind, payload) in [
        (
            "hold.released",
            json!({ "hold_ids": [hold_id], "reason": "mine now" }),
        ),
        ("hold.renewed", json!({ "hold_ids": [hold_id] })),
        ("hold.expired", json!({ "hold_id": hold_id })),
        (
            "resource.released",
            json!({ "lease_ids": [lease], "reason": "mine now" }),
        ),
        (
            "resource.renewed",
            json!({ "lease_ids": [lease], "ttl_seconds": 1 }),
        ),
        ("resource.lease_expired", json!({ "lease_id": lease })),
        (
            "lane.closed",
            json!({ "lane_id": lane, "status": "abandoned" }),
        ),
        (
            "lane.status_changed",
            json!({ "lane_id": lane, "to": "blocked" }),
        ),
        ("lane.updated", json!({ "lane_id": lane, "lead_id": bob })),
    ] {
        spoke.accepted(&s, kind, payload);
    }
    let holds = s.holds(HoldFilter::default()).unwrap();
    assert_eq!(holds.len(), 1);
    assert_eq!(
        holds[0].expires_at, hold_expiry,
        "`hold.renewed` didn't apply"
    );
    let after = s.resource(&ResourceRef::Id(r.resource_id)).unwrap();
    assert_eq!(after.leases.len(), 1);
    assert_eq!(after.leases[0].expires_at, r.leases[0].expires_at);
    let detail = s.lane(&lane, None).unwrap();
    assert_eq!(detail.lane.status, LaneStatus::Working);
    assert_eq!(detail.lane.lead, "alice@shop");
}

#[test]
fn a_spokes_own_objects_follow_its_events() {
    let (s, clock) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let eve = spoke_agent(&s, &spoke);
    assert_eq!(s.agent("eve@lab@evil").unwrap().program, "codex");

    // Sessions.
    spoke.accepted(&s, "session.ended", json!({ "agent_id": eve }));
    assert_eq!(s.agent("eve@lab@evil").unwrap().status, "offline");
    spoke.accepted(&s, "session.started", json!({ "agent_id": eve }));
    assert_eq!(s.agent("eve@lab@evil").unwrap().status, "active");

    // Holds are node-local, so the hub sees a spoke's only as warnings on its own acquires in
    // a project of the same name: what the warning shows is what the events left behind.
    s.ensure_project("t", "/hub/lab").unwrap();
    let warnings = |path: &str| {
        let r = s
            .acquire_holds(AcquireHolds {
                agent: "alice@shop".into(),
                project: "lab".into(),
                paths: vec![path.into()],
                ttl_seconds: Some(60),
                exclusive: true,
                reason: None,
                lane: None,
            })
            .unwrap();
        let ids: Vec<String> = r.granted.iter().map(|h| h.hold_id.clone()).collect();
        s.release_holds("alice@shop", &ids).unwrap();
        r.warnings
    };
    assert!(warnings("a.rs").is_empty());
    spoke.accepted(
        &s,
        "hold.acquired",
        json!({ "hold_id": "H1", "project_id": "EVIL-PROJECT", "agent_id": eve, "pattern": "a.rs", "ttl_seconds": 600, "lane_id": "NO-SUCH-LANE" }),
    );
    let seen = warnings("a.rs");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].held.agent, "eve@lab@evil");
    assert!(seen[0].held.exclusive, "exclusive is the default");
    assert_eq!(seen[0].held.lane_id, None, "an unknown lane isn't kept");

    // Renewal counts from the event's time: past the first expiry, it's still there.
    clock.fetch_add(500_000, Ordering::SeqCst);
    spoke.accepted(&s, "hold.renewed", json!({ "hold_ids": ["H1"] }));
    clock.fetch_add(500_000, Ordering::SeqCst);
    assert_eq!(warnings("a.rs").len(), 1);
    spoke.accepted(
        &s,
        "hold.released",
        json!({ "hold_ids": ["H1"], "reason": "done" }),
    );
    assert!(warnings("a.rs").is_empty());

    // A second hold ends by its own expiry event.
    spoke.accepted(
        &s,
        "hold.acquired",
        json!({ "hold_id": "H2", "project_id": "EVIL-PROJECT", "agent_id": eve, "pattern": "b.rs", "ttl_seconds": 600 }),
    );
    assert_eq!(warnings("b.rs").len(), 1);
    spoke.accepted(&s, "hold.expired", json!({ "hold_id": "H2" }));
    assert!(warnings("b.rs").is_empty());

    // Resources: registered with an owner, renewed, stopped.
    spoke.accepted(
        &s,
        "resource.registered",
        json!({ "resource_id": "R1", "kind": "model_server", "name": "ollama@1", "project_id": "EVIL-PROJECT", "lease_id": "RL1", "owner_id": eve, "ttl_seconds": 600, "port": 1, "pid": 2 }),
    );
    let id = ResourceRef::Id("R1".into());
    let r = s.resource(&id).unwrap();
    assert_eq!(r.node.as_deref(), Some("evil"));
    assert_eq!(r.leases[0].role, "owner");
    clock.fetch_add(60_000, Ordering::SeqCst);
    spoke.accepted(
        &s,
        "resource.renewed",
        json!({ "lease_ids": ["RL1"], "ttl_seconds": 1200 }),
    );
    assert!(s.resource(&id).unwrap().leases[0].expires_at > r.leases[0].expires_at);
    spoke.accepted(&s, "resource.stopped", json!({ "resource_id": "R1" }));
    assert_eq!(s.resource(&id).unwrap().status, "released");

    // A second resource: adopted, then its lease released; a third lease lapses.
    spoke.accepted(
        &s,
        "resource.registered",
        json!({ "resource_id": "R2", "kind": "model_server", "name": "ollama@2", "mode": "exclusive" }),
    );
    let id = ResourceRef::Id("R2".into());
    assert_eq!(s.resource(&id).unwrap().status, "lapsed");
    spoke.accepted(
        &s,
        "resource.adopted",
        json!({ "resource_id": "R2", "lease_id": "RL2", "agent_id": eve }),
    );
    assert_eq!(s.resource(&id).unwrap().status, "live");
    spoke.accepted(&s, "resource.released", json!({ "lease_ids": ["RL2"] }));
    assert_eq!(s.resource(&id).unwrap().status, "lapsed");
    spoke.accepted(
        &s,
        "resource.adopted",
        json!({ "resource_id": "R2", "lease_id": "RL3", "agent_id": eve }),
    );
    spoke.accepted(&s, "resource.lease_expired", json!({ "lease_id": "RL3" }));
    assert_eq!(s.resource(&id).unwrap().status, "lapsed");

    // An exclusive resource takes no borrowers; a released one neither.
    spoke.accepted(
        &s,
        "resource.borrowed",
        json!({ "resource_id": "R2", "lease_id": "RL4", "agent_id": eve }),
    );
    assert_eq!(s.resource(&id).unwrap().refcount, 0);
    spoke.accepted(
        &s,
        "resource.borrowed",
        json!({ "resource_id": "R1", "lease_id": "RL5", "agent_id": eve }),
    );
    assert_eq!(
        s.resource(&ResourceRef::Id("R1".into())).unwrap().refcount,
        0
    );

    // A lane mirror follows its status, lead and reports, and closes.
    spoke.accepted(
        &s,
        "lane.created",
        json!({ "lane_id": "LN1", "project_id": "EVIL-PROJECT", "lead_id": eve, "name": "ship", "status": "working" }),
    );
    let lane = s.lane("LN1", None).unwrap().lane;
    assert_eq!(
        (lane.status, lane.node.as_deref()),
        (LaneStatus::Working, Some("evil"))
    );
    spoke.accepted(
        &s,
        "lane.status_changed",
        json!({ "lane_id": "LN1", "to": "blocked" }),
    );
    assert_eq!(
        s.lane("LN1", None).unwrap().lane.status,
        LaneStatus::Blocked
    );
    spoke.accepted(&s, "lane.reported", json!({ "lane_id": "LN1" }));
    // A lead that isn't known here changes nothing.
    spoke.accepted(
        &s,
        "lane.updated",
        json!({ "lane_id": "LN1", "lead_id": "NOBODY" }),
    );
    assert_eq!(s.lane("LN1", None).unwrap().lane.lead, "eve@lab@evil");
    spoke.accepted(
        &s,
        "lane.closed",
        json!({ "lane_id": "LN1", "status": "done" }),
    );
    assert_eq!(s.lane("LN1", None).unwrap().lane.status, LaneStatus::Done);

    // Its own rename reaches the peer list.
    spoke.accepted(&s, "node.renamed", json!({ "display_name": "lab-box" }));
    assert_eq!(s.peers().unwrap()[0].display_name, "lab-box");
}

/// Malformed but signed events are stored and change nothing: nothing panics, and the rest of
/// the batch isn't lost.
#[test]
fn signed_events_with_missing_fields_change_nothing() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let before = s.agents().unwrap().len();
    for (kind, payload) in [
        ("agent.registered", json!({ "name": "nobody" })),
        (
            "message.sent",
            json!({ "subject": "no ids", "recipient_nodes": [s.node_id()] }),
        ),
        ("hold.acquired", json!({ "hold_id": "H" })),
        (
            "hold.acquired",
            json!({ "hold_id": "H", "project_id": "NO", "agent_id": "NO" }),
        ),
        ("hold.renewed", json!({})),
        ("hold.released", json!({ "hold_ids": ["NO"] })),
        ("hold.expired", json!({})),
        ("lane.created", json!({ "lane_id": "L" })),
        (
            "lane.created",
            json!({ "lane_id": "L", "project_id": "NO", "lead_id": "NO", "name": "x" }),
        ),
        ("lane.status_changed", json!({ "lane_id": "L" })),
        ("lane.closed", json!({ "lane_id": "L" })),
        ("lane.updated", json!({})),
        ("resource.registered", json!({})),
        ("resource.adopted", json!({})),
        (
            "resource.adopted",
            json!({ "resource_id": "NO", "lease_id": "NO", "agent_id": "NO" }),
        ),
        ("resource.borrowed", json!({})),
        ("resource.borrowed", json!({ "resource_id": "NO" })),
        ("resource.renewed", json!({})),
        ("resource.released", json!({})),
        ("resource.lease_expired", json!({})),
        ("resource.transferred", json!({})),
        ("resource.stopped", json!({})),
        ("decision.created", json!({})),
        ("decision.answered", json!({})),
        ("no.such_kind", json!({ "anything": true })),
    ] {
        spoke.accepted(&s, kind, payload);
    }
    assert_eq!(s.agents().unwrap().len(), before);
    assert!(s.holds(HoldFilter::default()).unwrap().is_empty());
}

// ---------------------------------------------------------------------------------------
// Store::ingest: what is refused before anything is applied.

#[test]
fn ingest_refuses_what_a_spoke_may_not_push() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &[]);
    assert!(matches!(
        s.ingest("STRANGER", &[], &[]),
        Err(Error::Forbidden(_))
    ));

    // Never shared, whoever signs it.
    for kind in [
        "token.created",
        "peer.paired",
        "invite.created",
        "summary.sent",
        "hold.conflict",
        "lane.hold_conflict",
        "resource.lease_expiring",
        "node.setting_changed",
    ] {
        let reason = spoke.rejected(&s, kind, json!({}));
        assert!(reason.contains("not shared"), "{kind}: {reason}");
    }
    // Mail to nowhere else isn't shared; mail elsewhere needs the permission.
    let reason = spoke.rejected(&s, "message.sent", json!({ "recipient_nodes": [EVIL] }));
    assert!(reason.contains("not shared"), "{reason}");
    let reason = spoke.rejected(
        &s,
        "message.sent",
        json!({ "recipient_nodes": [s.node_id()] }),
    );
    assert!(reason.contains("may not send mail"), "{reason}");
    for kind in [
        "decision.answer_relayed",
        "lane.report_proposed",
        "delivery.read",
    ] {
        let reason = spoke.rejected(&s, kind, json!({}));
        assert!(reason.contains("not shared"), "{kind}: {reason}");
    }

    // Not valid JSON.
    let mut bad = spoke.event(&s, "agent.registered", json!({}));
    bad.payload = "not json".into();
    let r = s.ingest(EVIL, &[bad], &[]).unwrap();
    assert!(r.rejected[0].reason.contains("invalid payload"), "{r:?}");

    // Claims to be the hub, or a node nobody has heard of.
    let mut own = spoke.event(&s, "agent.registered", json!({}));
    own.node_id = s.node_id().to_string();
    let r = s.ingest(EVIL, &[own], &[]).unwrap();
    assert!(
        r.rejected[0].reason.contains("come only from this node"),
        "{r:?}"
    );
    let mut other = spoke.event(&s, "agent.registered", json!({}));
    other.node_id = "THIRD-NODE".into();
    let r = s.ingest(EVIL, &[other], &[]).unwrap();
    assert!(
        r.rejected[0]
            .reason
            .contains("only push events they authored"),
        "{r:?}"
    );

    // Bad signature.
    let mut unsigned = spoke.event(&s, "agent.registered", json!({}));
    unsigned.sig = crypto::b64(&[0u8; 64]);
    let r = s.ingest(EVIL, &[unsigned], &[]).unwrap();
    assert!(
        r.rejected[0].reason.contains("bad event signature"),
        "{r:?}"
    );
    assert_eq!(r.accepted, 0);
}

#[test]
fn ingest_counts_duplicates_and_keeps_going_after_a_bad_event() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    let first = spoke.event(
        &s,
        "agent.registered",
        json!({ "agent_id": "A1", "name": "one" }),
    );
    let second = spoke.event(
        &s,
        "agent.registered",
        json!({ "agent_id": "A2", "name": "two" }),
    );
    let mut broken = spoke.event(
        &s,
        "agent.registered",
        json!({ "agent_id": "A3", "name": "three" }),
    );
    broken.sig = crypto::b64(&[0u8; 64]);
    let r = s
        .ingest(EVIL, &[first.clone(), broken, second.clone(), first], &[])
        .unwrap();
    assert_eq!(
        (r.accepted, r.duplicates, r.rejected.len()),
        (2, 1, 1),
        "{r:?}"
    );
    assert!(s.agent("one@@evil").is_ok());
    assert!(s.agent("two@@evil").is_ok());
    assert!(s.agent("three@@evil").is_err());

    // Mail from a sender nobody knows is stored but not projected, and still counted.
    let r = s
        .ingest(
            EVIL,
            &[spoke.event(
                &s,
                "message.sent",
                json!({ "message_id": "M", "sender_id": "UNKNOWN", "recipient_nodes": [s.node_id()] }),
            )],
            &[],
        )
        .unwrap();
    assert_eq!(r.accepted, 1, "{r:?}");
}

#[test]
fn a_revoked_spoke_can_neither_push_nor_pull() {
    let (s, _) = hub();
    let spoke = Spoke::paired_with(&s, &Perm::ALL);
    assert!(s.pull_for(EVIL, 0, 10).is_ok());
    s.revoke_peer("evil").unwrap();
    let ev = spoke.event(
        &s,
        "agent.registered",
        json!({ "agent_id": "A", "name": "a" }),
    );
    assert!(matches!(
        s.ingest(EVIL, &[ev], &[]),
        Err(Error::Forbidden(_))
    ));
    assert!(matches!(s.pull_for(EVIL, 0, 10), Err(Error::Forbidden(_))));
    assert!(matches!(
        s.pull_for("STRANGER", 0, 10),
        Err(Error::Forbidden(_))
    ));
}

#[test]
fn only_a_paired_spokes_signature_opens_the_federation_routes() {
    let (s, clock) = hub();
    let spoke = Spoke::paired_with(&s, &[Perm::ReadFeed]);
    let t = s.now();
    let sign = |path: &str, at: i64, body: &[u8]| {
        crypto::sign(&spoke.key, &crypto::request_message("POST", path, at, body))
    };
    let good = sign("/fed/push", t, b"body");
    assert_eq!(
        s.verify_peer_request(EVIL, "POST", "/fed/push", t, b"body", &good)
            .unwrap(),
        [Perm::ReadFeed]
    );
    let denied = |r: Result<Vec<Perm>>, what: &str| match r {
        Err(Error::Forbidden(m)) => m,
        other => panic!("{what}: {other:?}"),
    };
    // A different body, path or node doesn't verify.
    denied(
        s.verify_peer_request(EVIL, "POST", "/fed/push", t, b"other", &good),
        "body",
    );
    denied(
        s.verify_peer_request(EVIL, "POST", "/fed/pull", t, b"body", &good),
        "path",
    );
    denied(
        s.verify_peer_request("STRANGER", "POST", "/fed/push", t, b"body", &good),
        "node",
    );
    // Five minutes either way.
    for off in [5 * 60_000 + 1, -(5 * 60_000 + 1)] {
        let at = t + off;
        let sig = sign("/fed/push", at, b"body");
        let m = denied(
            s.verify_peer_request(EVIL, "POST", "/fed/push", at, b"body", &sig),
            "time",
        );
        assert!(m.contains("check the clocks"), "{m}");
    }
    let at = t + 5 * 60_000;
    let sig = sign("/fed/push", at, b"body");
    assert!(s
        .verify_peer_request(EVIL, "POST", "/fed/push", at, b"body", &sig)
        .is_ok());
    // A revoked spoke, with a fresh signature.
    s.revoke_peer("evil").unwrap();
    clock.fetch_add(1, Ordering::SeqCst);
    let t = s.now();
    let sig = sign("/fed/push", t, b"body");
    denied(
        s.verify_peer_request(EVIL, "POST", "/fed/push", t, b"body", &sig),
        "revoked",
    );
}

// ---------------------------------------------------------------------------------------
// Pairing input.

#[test]
fn node_names_and_invites_are_checked() {
    let (clock, handle) = Clock::manual(T0);
    let s = Store::open_in_memory(clock).unwrap();
    // An unnamed node can't invite.
    let e = s.create_invite("http://hub.test", &[], 60).unwrap_err();
    assert!(e.to_string().contains("unique name"), "{e}");
    let long = "x".repeat(41);
    for bad in ["", "two words", "a/b", "é", long.as_str()] {
        assert!(
            matches!(s.set_node_name(bad), Err(Error::Invalid(_))),
            "{bad:?}"
        );
    }
    assert_eq!(
        s.set_node_name(&"x".repeat(40)).unwrap().display_name.len(),
        40
    );
    s.set_node_name("hub-1.a_b").unwrap();

    for bad in ["hub.test", "ftp://hub.test", ""] {
        assert!(
            matches!(s.create_invite(bad, &[], 60), Err(Error::Invalid(_))),
            "{bad:?}"
        );
    }
    for bad in [0, -1] {
        assert!(
            matches!(
                s.create_invite("http://h", &[], bad),
                Err(Error::Invalid(_))
            ),
            "{bad}"
        );
    }
    // Permissions are deduplicated.
    let code = s
        .create_invite(
            "https://h",
            &[Perm::SendMail, Perm::SendMail, Perm::ReadFeed],
            60,
        )
        .unwrap();
    let invite = decode(&code);
    assert_eq!(invite.perms.len(), 2);
    assert_eq!(invite.hub.display_name, "hub-1.a_b");

    // The other side.
    let (clock2, _) = Clock::manual(T0);
    let spoke = Store::open_in_memory(clock2).unwrap();
    let unnamed = spoke.prepare_join(&code).unwrap_err();
    assert!(unnamed.to_string().contains("unique name"), "{unnamed}");
    spoke.set_node_name("hub-1.a_b").unwrap();
    let clash = spoke.prepare_join(&code).unwrap_err();
    assert!(matches!(clash, Error::Conflict(_)), "{clash}");
    spoke.set_node_name("laptop").unwrap();
    for bad in [
        "",
        "wyres-invite:abc",
        "another-invite:!!!",
        "another-invite:bm90IGpzb24",
    ] {
        assert!(
            matches!(spoke.prepare_join(bad), Err(Error::Invalid(_))),
            "{bad:?}"
        );
    }
    let (_, req) = spoke.prepare_join(&format!("  {code}\n")).unwrap();
    assert_eq!(req.node.display_name, "laptop");

    // Expired, measured on the hub's clock as well as the spoke's.
    handle.fetch_add(61_000, Ordering::SeqCst);
    assert!(matches!(s.accept_pairing(&req), Err(Error::Forbidden(_))));
}

#[test]
fn accepting_a_pairing_checks_the_invite_and_the_node() {
    let (s, _) = hub();
    let key = crypto::generate_key().unwrap();
    let entry = |id: &str, name: &str| NodeEntry {
        node_id: id.into(),
        display_name: name.into(),
        public_key: crypto::public_of(&key),
    };
    let invite = decode(&s.create_invite("http://hub.test", &[], 3600).unwrap());
    let req = |secret: &str, node: NodeEntry| PairRequest {
        invite_id: invite.invite_id.clone(),
        secret: secret.into(),
        node,
    };
    // Malformed node.
    assert!(matches!(
        s.accept_pairing(&req(&invite.secret, entry("N", "not valid"))),
        Err(Error::Invalid(_))
    ));
    let mut no_key = entry("N", "lab");
    no_key.public_key = "not a key".into();
    assert!(s.accept_pairing(&req(&invite.secret, no_key)).is_err());
    // Wrong secret; an invite that doesn't exist.
    assert!(matches!(
        s.accept_pairing(&req("wrong", entry("N", "lab"))),
        Err(Error::Forbidden(_))
    ));
    let mut unknown = req(&invite.secret, entry("N", "lab"));
    unknown.invite_id = "NOPE".into();
    assert!(matches!(
        s.accept_pairing(&unknown),
        Err(Error::Forbidden(_))
    ));
    // The hub's own name, and its own id.
    assert!(matches!(
        s.accept_pairing(&req(&invite.secret, entry("N", "hub"))),
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        s.accept_pairing(&req(&invite.secret, entry(s.node_id(), "lab"))),
        Err(Error::Conflict(_))
    ));
    // Refusals didn't use it up; a use does.
    s.accept_pairing(&req(&invite.secret, entry("N", "lab")))
        .unwrap();
    assert!(matches!(
        s.accept_pairing(&req(&invite.secret, entry("M", "lab2"))),
        Err(Error::Forbidden(_))
    ));
    // A second node can't take the name in use.
    let again = decode(&s.create_invite("http://hub.test", &[], 3600).unwrap());
    let clash = PairRequest {
        invite_id: again.invite_id,
        secret: again.secret,
        node: entry("M", "lab"),
    };
    assert!(matches!(s.accept_pairing(&clash), Err(Error::Conflict(_))));
}

#[test]
fn a_known_nodes_key_is_pinned_and_its_name_can_change() {
    let (hub, _) = hub();
    let s = receiver(&hub, &Perm::ALL);
    let key = crypto::public_of(&crypto::generate_key().unwrap());
    let other = crypto::public_of(&crypto::generate_key().unwrap());

    // A third node arrives through the paired hub's directory, then is renamed.
    let third = |name: &str, public_key: &str| NodeEntry {
        node_id: "THIRD".into(),
        display_name: name.into(),
        public_key: public_key.into(),
    };
    s.ingest(hub.node_id(), &[], &[third("third", &key)])
        .unwrap();
    s.ingest(hub.node_id(), &[], &[third("third-2", &key)])
        .unwrap();
    // Another key for it is refused, and so is a key that isn't one.
    assert!(matches!(
        s.ingest(hub.node_id(), &[], &[third("third-2", &other)]),
        Err(Error::Conflict(_))
    ));
    assert!(s
        .ingest(hub.node_id(), &[], &[third("third-2", "junk")])
        .is_err());
    // The hub's own entry is ignored, whatever it says.
    let mine = NodeEntry {
        node_id: s.node_id().to_string(),
        display_name: "not the hub".into(),
        public_key: "junk".into(),
    };
    s.ingest(hub.node_id(), &[], &[mine]).unwrap();
    assert_eq!(s.node_entry().unwrap().display_name, "receiver");
    // A renamed node's name can't be taken by this node.
    assert!(matches!(
        s.set_node_name("third-2"),
        Err(Error::Conflict(_))
    ));
}

#[test]
fn a_spoke_cannot_inject_a_node_directory() {
    let (s, _) = hub();
    Spoke::paired_with(&s, &Perm::ALL);
    let node = NodeEntry {
        node_id: "INJECTED".into(),
        display_name: "injected".into(),
        public_key: crypto::public_of(&crypto::generate_key().unwrap()),
    };
    assert!(matches!(
        s.ingest(EVIL, &[], &[node]),
        Err(Error::Forbidden(_))
    ));
    assert!(
        s.set_node_name("injected").is_ok(),
        "the directory was not changed"
    );
}

#[test]
fn completing_a_join_checks_who_answered() {
    let (hub_store, _) = hub();
    let (clock, _) = Clock::manual(T0);
    let spoke = Store::open_in_memory(clock).unwrap();
    spoke.set_node_name("laptop").unwrap();
    let code = hub_store
        .create_invite("http://hub.test", &[], 3600)
        .unwrap();
    let (invite, req) = spoke.prepare_join(&code).unwrap();
    let resp = hub_store.accept_pairing(&req).unwrap();

    // An answer from a different identity.
    let mut other_id = resp.clone();
    other_id.hub.node_id = "IMPOSTOR".into();
    assert!(matches!(
        spoke.complete_join(&invite, &other_id),
        Err(Error::Forbidden(_))
    ));
    let mut other_key = resp.clone();
    other_key.hub.public_key = crypto::public_of(&crypto::generate_key().unwrap());
    assert!(matches!(
        spoke.complete_join(&invite, &other_key),
        Err(Error::Forbidden(_))
    ));

    let peer = spoke.complete_join(&invite, &resp).unwrap();
    assert_eq!(peer.role, "hub");
    assert_eq!(
        spoke.hub().unwrap().map(|h| h.0),
        Some(hub_store.node_id().to_string())
    );
    // Joining the same hub again is allowed; a second hub isn't.
    spoke.complete_join(&invite, &resp).unwrap();
    let (clock, _) = Clock::manual(T0);
    let second = Store::open_in_memory(clock).unwrap();
    second.set_node_name("hub-two").unwrap();
    let second_invite = decode(&second.create_invite("http://two.test", &[], 3600).unwrap());
    let second_resp = PairResponse {
        hub: second_invite.hub.clone(),
        perms: vec![],
    };
    assert!(matches!(
        spoke.complete_join(&second_invite, &second_resp),
        Err(Error::Conflict(_))
    ));
    // A spoke doesn't serve pulls.
    assert!(matches!(
        spoke.pull_for(hub_store.node_id(), 0, 10),
        Err(Error::Forbidden(_))
    ));
}

#[test]
fn reachable_reads_hosts_and_ports_without_panicking() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    assert!(reachable(
        Some(&format!("http://127.0.0.1:{port}/path")),
        None
    ));
    assert!(reachable(Some(&format!("127.0.0.1:{port}")), None));
    assert!(reachable(Some("127.0.0.1"), Some(port)));
    assert!(reachable(Some(&format!("tcp://127.0.0.1:{port}")), Some(1)));
    // No endpoint, no port, or a host that doesn't resolve.
    assert!(!reachable(None, Some(port)));
    assert!(!reachable(Some("127.0.0.1"), None));
    assert!(!reachable(Some("http://[::1"), None));
    assert!(!reachable(Some(""), Some(port)));
    // IPv6 forms; whether ::1 answers depends on the machine, so only the parsing is at stake.
    let _ = reachable(Some(&format!("http://[::1]:{port}/")), None);
    let _ = reachable(Some("[::1]"), Some(port));
    drop(listener);
}
