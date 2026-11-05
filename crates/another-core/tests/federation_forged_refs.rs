//! Remote-agent state must survive events a paired spoke signs but has no say over. A
//! signature proves who sent an event, not that the sender owns what it names.
//!
//! These tests assert the effect on the hub, never whether the event was stored: a store-time
//! refusal of the same forgery satisfies them too (docs/design/07-remote-agents.md, Security).

use std::cell::Cell;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use another_core::*;
use serde_json::{json, Value};

const T0: i64 = 1_800_000_000_000;
const HOUR: i64 = 3_600_000;
const SECRET: &str = "the launch moves to Tuesday";

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
}

const SPOKE: &str = "SPOKE-NODE";
const SPOKE_PROJECT: &str = "SPOKE-PROJECT";
const EVE: &str = "SPOKE-AGENT";
const EVE_ADDRESS: &str = "eve@lab@spoke";

impl Spoke {
    /// Paired with every permission, with a project and an agent of its own on the hub.
    fn paired(hub: &Store) -> Spoke {
        let key = crypto::generate_key().unwrap();
        let code = hub
            .create_invite("http://hub.test", &Perm::ALL, 3600)
            .unwrap();
        let raw = code.strip_prefix("another-invite:").unwrap();
        let invite: Invite = serde_json::from_slice(&crypto::unb64(raw).unwrap()).unwrap();
        hub.accept_pairing(&PairRequest {
            invite_id: invite.invite_id,
            secret: invite.secret,
            node: NodeEntry {
                node_id: SPOKE.into(),
                display_name: "spoke".into(),
                public_key: crypto::public_of(&key),
            },
        })
        .unwrap();
        let spoke = Spoke {
            key,
            seq: Cell::new(0),
        };
        spoke.push(
            hub,
            "project.ensured",
            json!({ "project_id": SPOKE_PROJECT, "abs_path": "/src/lab", "slug": "lab" }),
        );
        spoke.push(
            hub,
            "agent.registered",
            json!({ "agent_id": EVE, "name": "eve", "project_id": SPOKE_PROJECT, "program": "codex" }),
        );
        spoke
    }

    fn push(&self, hub: &Store, kind: &str, payload: Value) {
        self.push_at(hub, hub.now(), kind, payload);
    }

    /// Push an event claiming timestamp `ts`. Whether it is stored or refused is not asserted.
    fn push_at(&self, hub: &Store, ts: i64, kind: &str, payload: Value) {
        self.seq.set(self.seq.get() + 1);
        let event_id = format!("{SPOKE}-EV-{}", self.seq.get());
        let payload = payload.to_string();
        let msg = crypto::event_message(&event_id, SPOKE, ts, "mallory", kind, &payload);
        let ev = WireEvent {
            seq: i64::from(self.seq.get()),
            event_id,
            node_id: SPOKE.into(),
            ts,
            actor: "mallory".into(),
            kind: kind.into(),
            payload,
            prev_id: None,
            sig: crypto::sign(&self.key, &msg),
        };
        hub.ingest(SPOKE, &[ev], &[]).unwrap();
    }
}

fn agent_id(s: &Store, who: &str) -> String {
    s.agent(who).unwrap().agent_id
}

fn shop_id(s: &Store) -> String {
    s.projects()
        .unwrap()
        .into_iter()
        .find(|p| p.slug == "shop")
        .unwrap()
        .project_id
}

fn secret_mail(s: &Store) -> Message {
    s.send_message(SendMessage {
        from: "alice@shop".into(),
        to: vec!["bob@shop".into()],
        subject: "private".into(),
        body: SECRET.into(),
        ..Default::default()
    })
    .unwrap()
}

fn unread(s: &Store, who: &str) -> u32 {
    s.inbox(who, InboxQuery::default()).unwrap().unread
}

fn lane(s: &Store, name: &str) -> String {
    s.create_lane(
        NewLane {
            agent: "alice@shop".into(),
            name: name.into(),
            project: "shop".into(),
            lead: "alice@shop".into(),
            charter: String::new(),
            status: Some(LaneStatus::Working),
        },
        None,
    )
    .unwrap()
    .lane
    .lane_id
}

fn join(s: &Store, agent: &str, lane: Option<&str>) -> Joined {
    let code = s
        .create_agent_invite(NewAgentInvite {
            agent: agent.into(),
            program: Program::Claude,
            project_globs: vec!["/work/shop".into()],
            lane: lane.map(str::to_string),
            expires_in_seconds: None,
            max_uses: None,
            ephemeral: true,
            idle_hours: Some(2),
            token_ttl_seconds: None,
        })
        .unwrap()
        .code;
    s.redeem_invite(
        &code,
        JoinClient {
            hostname: None,
            version: None,
        },
    )
    .unwrap()
}

/// Lane `train`, led by `first`, with `second` joined a minute later.
fn lane_with_two_members(s: &Store, clock: &AtomicI64) -> String {
    let id = lane(s, "train");
    join(s, "first", Some("train@shop"));
    clock.fetch_add(60_000, Ordering::SeqCst);
    join(s, "second", Some("train@shop"));
    s.update_lane(
        LaneUpdate {
            agent: "alice@shop".into(),
            lane: "train@shop".into(),
            lead: Some("first@shop".into()),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    id
}

// Mirrored decisions ---------------------------------------------------------------------

#[test]
fn a_mirrored_decision_does_not_copy_a_local_message_body() {
    let (s, _) = hub();
    let spoke = Spoke::paired(&s);
    let mail = secret_mail(&s);
    spoke.push(
        &s,
        "decision.created",
        json!({ "decision_id": "MIRROR", "asker_id": EVE, "project_id": SPOKE_PROJECT,
                "message_id": mail.message_id, "human": "reviewer", "option_values": ["yes"] }),
    );
    let question = s.decision("MIRROR", None).unwrap().question;
    assert!(
        question.starts_with("(question asked on node"),
        "{question}"
    );
}

// Local mail ------------------------------------------------------------------------------

#[test]
fn a_foreign_message_event_cannot_add_a_recipient_to_a_local_message() {
    let (s, _) = hub();
    let spoke = Spoke::paired(&s);
    let mail = secret_mail(&s);
    // An agent the spoke's operator can read as: it holds this agent's invite token.
    join(&s, "cloud", None);
    let cloud = agent_id(&s, "cloud@shop");
    spoke.push(
        &s,
        "message.sent",
        json!({ "message_id": mail.message_id, "sender_id": EVE, "subject": "x", "body": "x",
                "recipient_nodes": [s.node_id()],
                "recipients": [{ "agent_id": cloud, "node_id": s.node_id(), "role": "to" }] }),
    );
    let inbox = s.inbox("cloud@shop", InboxQuery::default()).unwrap();
    assert!(
        inbox.messages.iter().all(|m| m.message.body != SECRET),
        "{:?}",
        inbox.messages
    );
    assert_eq!(s.message(&mail.message_id).unwrap().body, SECRET);
}

#[test]
fn a_foreign_message_event_still_delivers_its_own_mail_on_a_retry() {
    let (s, _) = hub();
    let spoke = Spoke::paired(&s);
    let (alice, bob) = (agent_id(&s, "alice@shop"), agent_id(&s, "bob@shop"));
    for to in [&alice, &bob] {
        spoke.push(
            &s,
            "message.sent",
            json!({ "message_id": "OWN-MAIL", "sender_id": EVE, "subject": "hi", "body": "hi",
                    "recipient_nodes": [s.node_id()],
                    "recipients": [{ "agent_id": to, "node_id": s.node_id(), "role": "to" }] }),
        );
    }
    assert_eq!(unread(&s, "alice@shop"), 1);
    assert_eq!(unread(&s, "bob@shop"), 1);
}

#[test]
fn a_foreign_receipt_does_not_mark_a_local_agents_mail() {
    let (s, _) = hub();
    let spoke = Spoke::paired(&s);
    let mail = secret_mail(&s);
    let bob = agent_id(&s, "bob@shop");
    for kind in ["delivery.read", "delivery.acked"] {
        spoke.push(
            &s,
            kind,
            json!({ "message_id": mail.message_id, "agent_id": bob, "message_node": s.node_id() }),
        );
        assert_eq!(unread(&s, "bob@shop"), 1, "{kind}");
    }
}

#[test]
fn a_relayed_answer_does_not_mark_local_mail_read() {
    let (s, _) = hub();
    let spoke = Spoke::paired(&s);
    s.register_agent_trusted(RegisterAgent {
        name: "reviewer".into(),
        program: Program::Human,
        project: None,
        model: None,
        description: None,
    })
    .unwrap();
    let d = s
        .ask_decision(
            NewDecision {
                agent: "alice@shop".into(),
                human: Some("reviewer".into()),
                question: "Proceed?".into(),
                options: ["yes", "no"]
                    .map(|v| DecisionOption {
                        value: v.into(),
                        label: v.into(),
                        description: None,
                    })
                    .to_vec(),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    let mail = secret_mail(&s);
    spoke.push(
        &s,
        "decision.answer_relayed",
        json!({ "decision_node": s.node_id(), "decision_id": d.decision_id, "relayed_by_id": EVE,
                "text_message_id": mail.message_id, "value": "not-an-option" }),
    );
    assert_eq!(unread(&s, "bob@shop"), 1);
    assert_eq!(
        s.decision(&d.decision_id, None).unwrap().status,
        DecisionStatus::Open
    );
}

#[test]
fn a_mirrored_decision_does_not_land_in_a_project_of_this_node() {
    let (s, _) = hub();
    let spoke = Spoke::paired(&s);
    let lane_id = lane(&s, "train");
    let mail = secret_mail(&s);
    spoke.push(
        &s,
        "decision.created",
        json!({ "decision_id": "MIRROR", "asker_id": agent_id(&s, "alice@shop"),
                "project_id": shop_id(&s), "lane_id": lane_id,
                "message_id": mail.message_id, "human": "reviewer", "option_values": ["yes"] }),
    );
    let t = s
        .create_token("bob@shop", &["/work/shop".into()], None, None)
        .unwrap();
    let scope = s.verify_token(&t.secret).unwrap();
    assert_eq!(
        s.decision("MIRROR", Some(&scope)).unwrap_err().code(),
        "not_found"
    );
    assert!(s
        .decisions(DecisionFilter::default(), Some(&scope))
        .unwrap()
        .is_empty());
    assert!(s
        .decisions(
            DecisionFilter {
                lane: Some(lane_id),
                ..Default::default()
            },
            None
        )
        .unwrap()
        .is_empty());
}

#[test]
fn a_mirrored_decision_still_uses_its_own_nodes_question_mail() {
    let (s, _) = hub();
    let spoke = Spoke::paired(&s);
    let bob = agent_id(&s, "bob@shop");
    spoke.push(
        &s,
        "message.sent",
        json!({ "message_id": "QUESTION", "sender_id": EVE, "subject": "decision",
                "body": "Ship it?", "recipient_nodes": [s.node_id()],
                "recipients": [{ "agent_id": bob, "node_id": s.node_id(), "role": "to" }] }),
    );
    spoke.push(
        &s,
        "decision.created",
        json!({ "decision_id": "MIRROR", "asker_id": EVE, "project_id": SPOKE_PROJECT,
                "message_id": "QUESTION", "human": "reviewer", "option_values": ["yes"] }),
    );
    let d = s.decision("MIRROR", None).unwrap();
    assert_eq!(d.question, "Ship it?");
    assert_eq!(d.asker, EVE_ADDRESS);
    assert_eq!(d.project, "lab");
}

// Lane proposals --------------------------------------------------------------------------

#[test]
fn a_proposal_does_not_mark_local_mail_read() {
    let (s, _) = hub();
    let spoke = Spoke::paired(&s);
    let lane_id = lane(&s, "train");
    let mail = secret_mail(&s);
    let alice = agent_id(&s, "alice@shop");
    // Naming either the mail's real sender or the spoke's own agent, with a lane or without.
    for (agent, lane_named) in [
        (alice.as_str(), lane_id.as_str()),
        (EVE, lane_id.as_str()),
        (EVE, "NO-SUCH-LANE"),
    ] {
        spoke.push(
            &s,
            "lane.report_proposed",
            json!({ "lane_node": s.node_id(), "lane_id": lane_named, "agent_id": agent,
                    "summary_message_id": mail.message_id, "proposal_id": "P" }),
        );
        assert_eq!(
            unread(&s, "bob@shop"),
            1,
            "agent {agent}, lane {lane_named}"
        );
    }
    assert!(s.lane(&lane_id, None).unwrap().reports.is_empty());
}

#[test]
fn a_proposal_is_not_applied_as_an_agent_of_another_node() {
    let (s, _) = hub();
    let spoke = Spoke::paired(&s);
    let lane_id = lane(&s, "train");
    let alice = agent_id(&s, "alice@shop");
    spoke.push(
        &s,
        "message.sent",
        json!({ "message_id": "CARRIER", "sender_id": alice, "subject": "report",
                "body": "blocked, stop everything", "recipient_nodes": [s.node_id()],
                "recipients": [{ "agent_id": alice, "node_id": s.node_id(), "role": "to" }] }),
    );
    spoke.push(
        &s,
        "lane.report_proposed",
        json!({ "lane_node": s.node_id(), "lane_id": lane_id, "agent_id": alice,
                "status": "blocked", "summary_message_id": "CARRIER", "proposal_id": "P" }),
    );
    let detail = s.lane(&lane_id, None).unwrap();
    assert!(detail.reports.is_empty(), "{:?}", detail.reports);
    assert_eq!(detail.lane.status, LaneStatus::Working);
}

#[test]
fn a_proposal_from_the_nodes_own_agent_is_applied_and_its_carrier_read() {
    let (s, _) = hub();
    let spoke = Spoke::paired(&s);
    let lane_id = lane(&s, "train");
    let alice = agent_id(&s, "alice@shop");
    spoke.push(
        &s,
        "message.sent",
        json!({ "message_id": "CARRIER", "sender_id": EVE, "subject": "report",
                "body": "half way", "recipient_nodes": [s.node_id()],
                "recipients": [{ "agent_id": alice, "node_id": s.node_id(), "role": "to" }] }),
    );
    assert!(s
        .inbox("alice@shop", InboxQuery::default())
        .unwrap()
        .messages
        .iter()
        .any(|m| m.message.message_id == "CARRIER" && m.read_at.is_none()));
    spoke.push(
        &s,
        "lane.report_proposed",
        json!({ "lane_node": s.node_id(), "lane_id": lane_id, "agent_id": EVE,
                "summary_message_id": "CARRIER", "proposal_id": "P" }),
    );
    let detail = s.lane(&lane_id, None).unwrap();
    assert_eq!(detail.reports[0].summary, "half way");
    assert_eq!(detail.reports[0].agent, EVE_ADDRESS);
    assert!(s
        .inbox("alice@shop", InboxQuery::default())
        .unwrap()
        .messages
        .iter()
        .all(|m| m.message.message_id != "CARRIER" || m.read_at.is_some()));
}

// Lead handover ---------------------------------------------------------------------------

#[test]
fn a_foreign_hold_naming_a_local_agent_does_not_rank_for_lead() {
    let (s, clock) = hub();
    let spoke = Spoke::paired(&s);
    let lane_id = lane_with_two_members(&s, &clock);
    // Older than every member, and alive past the sweep through its own expiry.
    spoke.push_at(
        &s,
        1,
        "hold.acquired",
        json!({ "hold_id": "FORGED-HOLD", "project_id": shop_id(&s),
                "agent_id": agent_id(&s, "bob@shop"), "lane_id": lane_id, "pattern": "src/x.rs",
                "expires_at": T0 + 24 * HOUR }),
    );
    s.sweep().unwrap();
    s.leave("first@shop").unwrap();
    assert_eq!(s.lane("train@shop", None).unwrap().lane.lead, "second@shop");
}

#[test]
fn a_foreign_event_cannot_move_a_local_hold_into_a_lane() {
    let (s, clock) = hub();
    let spoke = Spoke::paired(&s);
    // Bob's real hold, in no lane, older than either member's membership.
    let hold = s
        .acquire_holds(AcquireHolds {
            agent: "bob@shop".into(),
            project: "shop".into(),
            paths: vec!["src/x.rs".into()],
            ttl_seconds: Some(4 * 3600),
            exclusive: true,
            reason: None,
            lane: None,
        })
        .unwrap()
        .granted[0]
        .hold_id
        .clone();
    clock.fetch_add(60_000, Ordering::SeqCst);
    let lane_id = lane_with_two_members(&s, &clock);
    spoke.push(
        &s,
        "hold.updated",
        json!({ "hold_id": hold, "project_id": shop_id(&s),
                "agent_id": agent_id(&s, "bob@shop"), "lane_id": lane_id, "pattern": "src/x.rs",
                "expires_at": T0 + 24 * HOUR }),
    );
    s.leave("first@shop").unwrap();
    assert_eq!(s.lane("train@shop", None).unwrap().lane.lead, "second@shop");
}

#[test]
fn a_local_hold_in_the_lane_still_ranks_for_lead() {
    let (s, clock) = hub();
    let _spoke = Spoke::paired(&s);
    lane(&s, "train");
    s.acquire_holds(AcquireHolds {
        agent: "bob@shop".into(),
        project: "shop".into(),
        paths: vec!["src/x.rs".into()],
        ttl_seconds: Some(4 * 3600),
        exclusive: true,
        reason: None,
        lane: Some("train@shop".into()),
    })
    .unwrap();
    clock.fetch_add(60_000, Ordering::SeqCst);
    join(&s, "first", Some("train@shop"));
    s.update_lane(
        LaneUpdate {
            agent: "alice@shop".into(),
            lane: "train@shop".into(),
            lead: Some("first@shop".into()),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    s.leave("first@shop").unwrap();
    assert_eq!(s.lane("train@shop", None).unwrap().lane.lead, "bob@shop");
}

// Ephemeral departure ---------------------------------------------------------------------

fn assert_still_joined(s: &Store, joined: &Joined) {
    s.sweep().unwrap();
    assert!(s.verify_token(&joined.token).is_ok());
    assert_ne!(s.remote_agents().unwrap()[0].presence, Presence::Gone);
    assert!(!s
        .events(500)
        .unwrap()
        .iter()
        .any(|e| e.kind == "lane.agent_left" || e.kind == "agent.gone"));
}

#[test]
fn a_foreign_session_end_does_not_depart_a_local_ephemeral_agent() {
    let (s, _) = hub();
    let spoke = Spoke::paired(&s);
    let joined = join(&s, "cloud", None);
    s.heartbeat("cloud@shop", false).unwrap();
    let cloud = agent_id(&s, "cloud@shop");
    spoke.push(&s, "session.ended", json!({ "agent_id": cloud }));
    assert_still_joined(&s, &joined);
}

#[test]
fn a_foreign_session_event_cannot_backdate_a_local_agent_into_idle_expiry() {
    let (s, _) = hub();
    let spoke = Spoke::paired(&s);
    let joined = join(&s, "cloud", None);
    s.heartbeat("cloud@shop", false).unwrap();
    let cloud = agent_id(&s, "cloud@shop");
    spoke.push_at(&s, 1, "session.started", json!({ "agent_id": cloud }));
    assert_still_joined(&s, &joined);
}

#[test]
fn a_foreign_registration_cannot_backdate_or_rename_a_local_agent() {
    let (s, _) = hub();
    let spoke = Spoke::paired(&s);
    let joined = join(&s, "cloud", None);
    s.heartbeat("cloud@shop", false).unwrap();
    let cloud = agent_id(&s, "cloud@shop");
    spoke.push_at(
        &s,
        1,
        "agent.registered",
        json!({ "agent_id": cloud, "name": "cloud", "project_id": SPOKE_PROJECT, "program": "codex" }),
    );
    assert_still_joined(&s, &joined);
    let a = s.agent("cloud@shop").unwrap();
    assert_eq!(a.address, "cloud@shop");
    assert_eq!(a.program, "claude");
}

#[test]
fn a_nodes_own_session_events_still_set_its_agents_status() {
    let (s, _) = hub();
    let spoke = Spoke::paired(&s);
    spoke.push(&s, "session.ended", json!({ "agent_id": EVE }));
    assert_eq!(s.agent(EVE_ADDRESS).unwrap().status, "offline");
    spoke.push(&s, "session.started", json!({ "agent_id": EVE }));
    assert_eq!(s.agent(EVE_ADDRESS).unwrap().status, "active");
}

#[test]
fn a_local_session_end_still_departs_an_ephemeral_agent() {
    let (s, _) = hub();
    let _spoke = Spoke::paired(&s);
    let joined = join(&s, "cloud", None);
    s.heartbeat("cloud@shop", true).unwrap();
    s.sweep().unwrap();
    assert!(s.verify_token(&joined.token).is_err());
    assert_eq!(s.remote_agents().unwrap()[0].presence, Presence::Gone);
}

#[test]
fn a_foreign_message_cannot_be_sent_as_another_nodes_agent() {
    let (s, _) = hub();
    let spoke = Spoke::paired(&s);
    let (alice, bob) = (agent_id(&s, "alice@shop"), agent_id(&s, "bob@shop"));
    spoke.push(
        &s,
        "message.sent",
        json!({ "message_id": "FORGED-FROM", "sender_id": alice, "subject": "pay this",
                "body": "x", "recipient_nodes": [s.node_id()],
                "recipients": [{ "agent_id": bob, "node_id": s.node_id(), "role": "to" }] }),
    );
    assert!(s.message("FORGED-FROM").is_err());
    assert_eq!(unread(&s, "bob@shop"), 0);
}

#[test]
fn a_foreign_message_in_a_learned_thread_discloses_nothing_to_its_recipient() {
    let (s, _) = hub();
    let spoke = Spoke::paired(&s);
    let mail = secret_mail(&s);
    join(&s, "cloud", None);
    let cloud = agent_id(&s, "cloud@shop");
    spoke.push(
        &s,
        "message.sent",
        json!({ "message_id": "FORGED-JOIN", "thread_id": mail.thread_id, "sender_id": EVE,
                "subject": "hello", "body": "hello", "recipient_nodes": [s.node_id()],
                "recipients": [{ "agent_id": cloud, "node_id": s.node_id(), "role": "to" }] }),
    );
    for who in [cloud.as_str(), EVE] {
        let visible = s.thread_for(who, &mail.thread_id).unwrap();
        assert_eq!(visible.messages.len(), 1);
        assert_eq!(visible.messages[0].message_id, "FORGED-JOIN");
    }
    let listed = s.threads_for("cloud@shop", 20).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].subject, "hello");
    assert_eq!(listed[0].messages, 1);
    assert!(!listed[0].participants.iter().any(|p| p == "bob@shop"));
    // Local readers are trusted and keep the whole thread.
    assert!(s
        .thread(&mail.thread_id)
        .unwrap()
        .messages
        .iter()
        .any(|m| m.body == SECRET));
}

#[test]
fn a_spoke_agents_reply_still_lands_in_the_thread_and_reads_its_own_part() {
    let (s, _) = hub();
    let spoke = Spoke::paired(&s);
    let question = s
        .send_message(SendMessage {
            from: "alice@shop".into(),
            to: vec![EVE_ADDRESS.into()],
            subject: "question".into(),
            body: "ready?".into(),
            ..Default::default()
        })
        .unwrap();
    let alice = agent_id(&s, "alice@shop");
    spoke.push(
        &s,
        "message.sent",
        json!({ "message_id": "EVE-REPLY", "thread_id": question.thread_id,
                "reply_to": question.message_id, "sender_id": EVE, "subject": "Re: question",
                "body": "yes", "recipient_nodes": [s.node_id()],
                "recipients": [{ "agent_id": alice, "node_id": s.node_id(), "role": "to" }] }),
    );
    assert_eq!(
        s.message("EVE-REPLY").unwrap().thread_id,
        question.thread_id
    );
    assert_eq!(
        s.thread_for(EVE, &question.thread_id)
            .unwrap()
            .messages
            .len(),
        2
    );
    assert!(s.message_visible_to(EVE, &question.message_id).unwrap());
    assert!(!s
        .message_visible_to(&agent_id(&s, "bob@shop"), &question.message_id)
        .unwrap());
    assert!(!s.message_visible_to(EVE, "NO-SUCH-ID").unwrap());
    assert!(matches!(
        s.thread_for(&agent_id(&s, "bob@shop"), &question.thread_id),
        Err(Error::NotFound(_))
    ));
}
