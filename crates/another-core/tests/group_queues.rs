//! Group work queues: requests addressed to a group are queue items that members pull with
//! `request next`, atomically, in priority order; every transition is an audit event.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use another_core::*;
use serde_json::Value;

const T0: i64 = 1_800_000_000_000;

struct Fx {
    _dir: tempfile::TempDir,
    home: PathBuf,
    shop: String,
    store: Store,
    clock: Arc<AtomicI64>,
}

impl Fx {
    fn new() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let shop = dir.path().join("shop");
        std::fs::create_dir_all(&shop).unwrap();
        let shop = shop.to_string_lossy().into_owned();
        let (clock, handle) = Clock::manual(T0);
        let home = dir.path().join("home");
        let store = Store::open_with_clock(&home, clock).unwrap();
        for name in ["lead", "alice", "bob", "carol"] {
            store
                .register_agent(RegisterAgent {
                    name: name.into(),
                    program: Program::Claude,
                    project: Some(shop.clone()),
                    model: None,
                    description: None,
                })
                .unwrap();
        }
        Fx {
            _dir: dir,
            home,
            shop,
            store,
            clock: handle,
        }
    }

    fn advance(&self, secs: i64) {
        self.clock.fetch_add(secs * 1000, Ordering::SeqCst);
    }

    /// `reviewers@shop` with alice and bob (created by alice).
    fn reviewers(&self) -> Group {
        self.store
            .create_group("alice", &self.shop, "reviewers")
            .unwrap();
        self.store.join_group("bob", "reviewers", None).unwrap()
    }

    fn queue(&self, subject: &str, priority: Priority, due: Option<i64>) -> String {
        self.queue_to(&["group:reviewers"], subject, priority, due)
    }

    fn queue_to(&self, to: &[&str], subject: &str, priority: Priority, due: Option<i64>) -> String {
        self.store
            .send_message(SendMessage {
                from: "lead".into(),
                to: to.iter().map(|s| s.to_string()).collect(),
                subject: subject.into(),
                body: format!("body of {subject}"),
                request: Some(NewRequest {
                    due_in_seconds: due,
                    priority,
                }),
                ..Default::default()
            })
            .unwrap()
            .request_id
            .unwrap()
    }

    fn next(&self, agent: &str) -> Option<Request> {
        self.next_with(agent, false)
    }

    fn next_with(&self, agent: &str, include_stale: bool) -> Option<Request> {
        self.store
            .request_next(NextRequest {
                agent: agent.into(),
                include_stale,
                ..Default::default()
            })
            .unwrap()
    }

    fn act(
        &self,
        agent: &str,
        rid: &str,
        action: RequestAction,
        due: Option<i64>,
    ) -> Result<Request> {
        self.store.update_request(RequestUpdate {
            agent: agent.into(),
            request_id: rid.into(),
            action,
            note: None,
            due_in_seconds: due,
        })
    }

    fn events(&self) -> Vec<Event> {
        self.store.events(100_000).unwrap()
    }

    fn event_count(&self) -> usize {
        self.events().len()
    }

    fn request_events(&self, rid: &str) -> Vec<(String, Value)> {
        self.events()
            .into_iter()
            .filter(|e| e.payload.get("request_id").and_then(Value::as_str) == Some(rid))
            .map(|e| (e.kind, e.payload))
            .collect()
    }

    /// (role, read?) of an agent's delivery of a request's message, if any.
    fn delivery(&self, rid: &str, agent: &str) -> Option<(String, bool)> {
        let db = rusqlite::Connection::open(self.home.join("another.db")).unwrap();
        db.query_row(
            "SELECT d.role, d.read_at IS NOT NULL FROM deliveries d
             JOIN requests r ON r.message_id = d.message_id
             JOIN agents a ON a.agent_id = d.agent_id
             WHERE r.request_id = ?1 AND a.name = ?2",
            [rid, agent],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()
    }

    fn deliveries_for(&self, rid: &str) -> usize {
        let db = rusqlite::Connection::open(self.home.join("another.db")).unwrap();
        db.query_row(
            "SELECT count(*) FROM deliveries d JOIN requests r ON r.message_id = d.message_id
             WHERE r.request_id = ?1",
            [rid],
            |r| r.get::<_, i64>(0),
        )
        .unwrap() as usize
    }
}

#[test]
fn a_group_request_is_a_queue_item_not_a_broadcast() {
    let fx = Fx::new();
    let g = fx.reviewers();
    assert_eq!(g.address, format!("reviewers@{}", g.project));
    assert_eq!(g.members.len(), 2);
    let rid = fx.queue("Review #143", Priority::High, None);
    let req = fx.store.request(&rid).unwrap();
    assert_eq!(req.status, RequestStatus::Open);
    assert_eq!(req.owner, None);
    assert_eq!(req.priority, Priority::High);
    assert_eq!(req.group.as_deref(), Some(g.address.as_str()));
    assert_eq!(
        fx.deliveries_for(&rid),
        0,
        "no member gets mail for a queue item"
    );
    for member in ["alice", "bob"] {
        let inbox = fx.store.inbox(member, InboxQuery::default()).unwrap();
        assert_eq!(inbox.unread, 0);
        assert!(inbox.requests.is_empty());
    }
    let created = fx.request_events(&rid);
    let (_, p) = created
        .iter()
        .find(|(k, _)| k == "request.created")
        .unwrap();
    assert_eq!(p["group_id"], g.group_id);
    assert_eq!(p["priority"], "high");
    let groups = fx.store.agent_groups("bob").unwrap();
    assert_eq!(groups[0].queue.open, 1);
    assert_eq!(groups[0].queue.open_by_priority, [0, 1, 0, 0]);
}

#[test]
fn plain_mail_to_a_group_and_two_groups_are_refused() {
    let fx = Fx::new();
    fx.reviewers();
    let plain = fx.store.send_message(SendMessage {
        from: "lead".into(),
        to: vec!["group:reviewers".into()],
        subject: "hi all".into(),
        body: "b".into(),
        ..Default::default()
    });
    assert!(matches!(plain, Err(Error::Invalid(_))), "{plain:?}");
    fx.store.create_group("carol", &fx.shop, "testers").unwrap();
    let two = fx.store.send_message(SendMessage {
        from: "lead".into(),
        to: vec!["group:reviewers".into(), "group:testers".into()],
        subject: "x".into(),
        body: "b".into(),
        request: Some(NewRequest::default()),
        ..Default::default()
    });
    assert!(matches!(two, Err(Error::Invalid(_))), "{two:?}");
    let cc = fx.store.send_message(SendMessage {
        from: "lead".into(),
        to: vec!["alice".into()],
        cc: vec!["group:reviewers".into()],
        subject: "x".into(),
        body: "b".into(),
        request: Some(NewRequest::default()),
        ..Default::default()
    });
    assert!(matches!(cc, Err(Error::Invalid(_))), "{cc:?}");
}

#[test]
fn next_takes_priority_then_due_then_age_then_id() {
    let fx = Fx::new();
    fx.reviewers();
    let low = fx.queue("low", Priority::Low, None);
    fx.advance(1);
    let normal_late = fx.queue("normal, no due", Priority::Normal, None);
    fx.advance(1);
    let normal_due = fx.queue("normal, due", Priority::Normal, Some(3600));
    fx.advance(1);
    let urgent = fx.queue("urgent", Priority::Urgent, None);
    let order: Vec<String> =
        std::iter::from_fn(|| fx.next("alice").map(|r| r.request_id)).collect();
    assert_eq!(order, [urgent, normal_due, normal_late, low]);
    assert!(fx.next("bob").is_none());
}

#[test]
fn items_queued_in_the_same_millisecond_come_out_in_send_order() {
    // Request ids are ULIDs, whose tails are random within one millisecond, so
    // ordering by id alone would shuffle items that share a `created_at`.
    let fx = Fx::new();
    fx.reviewers();
    let sent: Vec<String> = (0..20)
        .map(|i| fx.queue(&format!("item {i}"), Priority::Normal, None))
        .collect();
    let order: Vec<String> =
        std::iter::from_fn(|| fx.next("alice").map(|r| r.request_id)).collect();
    assert_eq!(order, sent);
}

#[test]
fn stale_items_are_taken_only_when_asked_and_after_open_ones_of_their_priority() {
    let fx = Fx::new();
    fx.reviewers();
    let urgent_stale = fx.queue("urgent, will go stale", Priority::Urgent, Some(60));
    let normal_stale = fx.queue("normal, will go stale", Priority::Normal, Some(60));
    let claimed = fx.next("bob").unwrap();
    assert_eq!(claimed.request_id, urgent_stale);
    fx.act("bob", &normal_stale, RequestAction::Claim, None)
        .unwrap();
    fx.advance(120);
    fx.store.sweep().unwrap();
    let normal_open = fx.queue("normal, open", Priority::Normal, None);
    assert_eq!(
        fx.next("alice").unwrap().request_id,
        normal_open,
        "stale is skipped by default"
    );
    let normal_open2 = fx.queue("normal, open 2", Priority::Normal, None);
    let taken = fx.next_with("alice", true).unwrap();
    assert_eq!(
        taken.request_id, urgent_stale,
        "an urgent stale item beats a normal open one"
    );
    assert_eq!(
        fx.next_with("alice", true).unwrap().request_id,
        normal_open2,
        "open before stale"
    );
    let last = fx.next_with("alice", true).unwrap();
    assert_eq!(last.request_id, normal_stale);
    assert_eq!(last.due_at, None, "a past deadline is cleared on takeover");
    let ev = fx.request_events(&normal_stale);
    let (_, p) = ev
        .iter()
        .rev()
        .find(|(k, _)| k == "request.claimed")
        .unwrap();
    assert_eq!(p["via"], "next");
    assert!(
        !p["previous_owner_id"].is_null(),
        "a takeover names the previous owner"
    );
}

#[test]
fn only_members_pull_or_claim_and_leaving_keeps_owned_items() {
    let fx = Fx::new();
    fx.reviewers();
    let a = fx.queue("a", Priority::Normal, None);
    let b = fx.queue("b", Priority::Normal, None);
    assert!(fx.next("carol").is_none(), "carol is in no group");
    let named = fx.store.request_next(NextRequest {
        agent: "carol".into(),
        group: Some("reviewers".into()),
        ..Default::default()
    });
    assert!(matches!(named, Err(Error::Forbidden(_))), "{named:?}");
    let by_id = fx.act("carol", &a, RequestAction::Claim, None);
    assert!(matches!(by_id, Err(Error::Forbidden(_))), "{by_id:?}");

    assert_eq!(fx.next("bob").unwrap().request_id, a);
    fx.store.leave_group("bob", "reviewers", None).unwrap();
    assert!(fx.next("bob").is_none(), "no new pulls after leaving");
    let done = fx.act("bob", &a, RequestAction::Complete, None).unwrap();
    assert_eq!(
        done.status,
        RequestStatus::Done,
        "an owned item stays the leaver's"
    );

    let refused = fx.store.delete_group("alice", "reviewers");
    assert!(matches!(refused, Err(Error::Conflict(_))), "{refused:?}");
    fx.act("lead", &b, RequestAction::Cancel, None).unwrap();
    fx.store.delete_group("alice", "reviewers").unwrap();
    assert!(fx.store.groups(None).unwrap().is_empty());
}

#[test]
fn the_claimer_delivery_is_idempotent_and_leaves_direct_mail_alone() {
    let fx = Fx::new();
    fx.reviewers();
    // First claim: one read, announced `queue` delivery; no hook would announce it.
    let rid = fx.queue("first", Priority::Normal, None);
    fx.next("alice").unwrap();
    assert_eq!(fx.delivery(&rid, "alice"), Some(("queue".into(), true)));
    assert_eq!(fx.store.unannounced("alice", "", 100).unwrap().count, 0);
    // Re-claim by the same agent after a release: the same single row.
    fx.act("alice", &rid, RequestAction::Release, None).unwrap();
    fx.act("alice", &rid, RequestAction::Claim, None).unwrap();
    assert_eq!(fx.deliveries_for(&rid), 1);
    // Another member after a release: its own row; the earlier claimer keeps hers.
    fx.act("alice", &rid, RequestAction::Release, None).unwrap();
    fx.next("bob").unwrap();
    assert_eq!(fx.delivery(&rid, "bob"), Some(("queue".into(), true)));
    assert_eq!(fx.delivery(&rid, "alice"), Some(("queue".into(), true)));
    // A direct recipient (unread) claims through the group: its mail stays unread and
    // announceable, and the claim does not fail on the existing delivery.
    let mixed = fx.queue_to(&["group:reviewers", "bob"], "mixed", Priority::Urgent, None);
    assert_eq!(fx.delivery(&mixed, "bob"), Some(("to".into(), false)));
    let got = fx.next("bob").unwrap();
    assert_eq!(got.request_id, mixed);
    assert_eq!(fx.delivery(&mixed, "bob"), Some(("to".into(), false)));
    assert_eq!(fx.store.unannounced("bob", "", 100).unwrap().count, 1);
    // A queue delivery does not make a released item show in the old claimer's inbox.
    fx.act("bob", &mixed, RequestAction::Release, None).unwrap();
    let inbox = fx.store.inbox("alice", InboxQuery::default()).unwrap();
    assert!(inbox.requests.iter().all(|r| r.request_id != rid));
}

#[test]
fn deadlines_on_queue_items() {
    let fx = Fx::new();
    fx.reviewers();
    let future = fx.queue("future due", Priority::Normal, Some(3600));
    let got = fx.next("alice").unwrap();
    assert_eq!(got.request_id, future);
    assert!(got.due_at.is_some(), "a future deadline is kept");
    // Re-claim by the owner with a new deadline is recorded.
    let before = fx.request_events(&future).len();
    fx.act("alice", &future, RequestAction::Claim, Some(86_400))
        .unwrap();
    let ev = fx.request_events(&future);
    assert_eq!(ev.len(), before + 1);
    assert_eq!(ev.last().unwrap().0, "request.updated");
    // Release after the deadline passed puts it back as open, not stale again.
    fx.advance(90_000);
    fx.store.sweep().unwrap();
    assert_eq!(
        fx.store.request(&future).unwrap().status,
        RequestStatus::Stale
    );
    let released = fx
        .act("alice", &future, RequestAction::Release, None)
        .unwrap();
    assert_eq!(released.due_at, None);
    fx.advance(1);
    fx.store.sweep().unwrap();
    assert_eq!(
        fx.store.request(&future).unwrap().status,
        RequestStatus::Open
    );
    assert_eq!(fx.next("bob").unwrap().request_id, future);
}

#[test]
fn an_empty_poll_writes_nothing_of_its_own() {
    let fx = Fx::new();
    fx.reviewers();
    let before = fx.event_count();
    assert!(fx.next("alice").is_none());
    assert_eq!(
        fx.event_count(),
        before,
        "nothing eligible, nothing overdue: no event"
    );
    // An unrelated direct request goes overdue: the shared sweep records that, once.
    let direct = fx
        .store
        .send_message(SendMessage {
            from: "lead".into(),
            to: vec!["carol".into()],
            subject: "direct".into(),
            body: "b".into(),
            request: Some(NewRequest {
                due_in_seconds: Some(60),
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap()
        .request_id
        .unwrap();
    fx.advance(120);
    let before = fx.event_count();
    assert!(fx.next("alice").is_none());
    let new: Vec<String> = fx.events()[before..]
        .iter()
        .map(|e| e.kind.clone())
        .collect();
    assert_eq!(new, ["request.stale"]);
    assert_eq!(
        fx.store.request(&direct).unwrap().status,
        RequestStatus::Stale
    );
    let before = fx.event_count();
    assert!(fx.next("alice").is_none());
    assert_eq!(fx.event_count(), before, "a second poll writes nothing");
}

#[test]
fn completion_is_audited_with_group_priority_and_work_time() {
    let fx = Fx::new();
    let g = fx.reviewers();
    let rid = fx.queue("work", Priority::High, None);
    fx.next("alice").unwrap();
    fx.advance(840);
    fx.act("alice", &rid, RequestAction::Complete, None)
        .unwrap();
    let ev: Vec<(String, Value)> = fx
        .request_events(&rid)
        .into_iter()
        .filter(|(k, _)| k.starts_with("request."))
        .collect();
    let kinds: Vec<&str> = ev.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        kinds,
        ["request.created", "request.claimed", "request.completed"]
    );
    for (kind, p) in &ev {
        assert_eq!(p["group_id"], g.group_id, "{kind}");
        assert_eq!(p["priority"], "high", "{kind}");
    }
    let (_, done) = &ev[2];
    assert_eq!(done["duration_ms"], 840_000);
    assert!(done["claimed_at"].is_string());
    // The creator gets its Done: mail, as for any request.
    let lead = fx.store.inbox("lead", InboxQuery::default()).unwrap();
    assert!(lead
        .messages
        .iter()
        .any(|m| m.message.subject == "Done: work"));

    // Completed without a claim: no work time rather than zero.
    let direct = fx.queue("quick", Priority::Normal, None);
    fx.act("bob", &direct, RequestAction::Complete, None)
        .unwrap();
    let (_, p) = fx
        .request_events(&direct)
        .into_iter()
        .find(|(k, _)| k == "request.completed")
        .unwrap();
    assert!(p["duration_ms"].is_null());
    assert!(p["claimed_at"].is_null());
}

#[test]
fn listing_by_group() {
    let fx = Fx::new();
    fx.reviewers();
    fx.store.create_group("carol", &fx.shop, "testers").unwrap();
    let a = fx.queue("a", Priority::Normal, None);
    fx.queue_to(&["group:testers"], "t", Priority::Normal, None);
    let listed = fx
        .store
        .requests(RequestFilter {
            group: Some(format!(
                "reviewers@{}",
                fx.store.groups(None).unwrap()[0].project
            )),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].request_id, a);
}

/// Separate connections in threads (one `Store` each) on one home: every item is claimed
/// exactly once. (Real separate processes are covered by the binary test.)
#[test]
fn concurrent_next_never_hands_out_an_item_twice() {
    let fx = Fx::new();
    fx.reviewers();
    for name in ["dave", "erin", "frank", "gina", "hank", "ivy"] {
        fx.store
            .register_agent(RegisterAgent {
                name: name.into(),
                program: Program::Codex,
                project: Some(fx.shop.clone()),
                model: None,
                description: None,
            })
            .unwrap();
        fx.store.join_group(name, "reviewers", None).unwrap();
    }
    let total = 200;
    let ids: HashSet<String> = (0..total)
        .map(|i| fx.queue(&format!("item {i}"), Priority::Normal, None))
        .collect();
    let agents = [
        "alice", "bob", "dave", "erin", "frank", "gina", "hank", "ivy",
    ];
    let handles: Vec<_> = agents
        .iter()
        .map(|agent| {
            let home = fx.home.clone();
            let agent = agent.to_string();
            std::thread::spawn(move || {
                let store = Store::open(&home).unwrap();
                let mut mine = Vec::new();
                loop {
                    match store.request_next(NextRequest {
                        agent: agent.clone(),
                        ..Default::default()
                    }) {
                        Ok(Some(r)) => mine.push(r.request_id),
                        Ok(None) => break,
                        Err(e) => panic!("{agent}: {e:?}"),
                    }
                }
                mine
            })
        })
        .collect();
    let mut seen = Vec::new();
    for h in handles {
        seen.extend(h.join().unwrap());
    }
    let unique: HashSet<String> = seen.iter().cloned().collect();
    assert_eq!(seen.len(), total, "every item handed out once");
    assert_eq!(unique, ids, "no item twice, none missed");
    let claims = fx
        .events()
        .into_iter()
        .filter(|e| e.kind == "request.claimed")
        .count();
    assert_eq!(claims, total);
}

/// Adversarial F1 (review of #152): QUEUES.md and D77 say queue items "stay on this node
/// and are never sent to peers". Nothing of a queue item may be offered to a hub: no event
/// that names its group, and not its subject.
#[test]
fn adv152_queue_item_events_never_leave_the_node() {
    let fx = Fx::new();
    fx.reviewers();
    let rid = fx.queue("Review the secret migration", Priority::High, None);
    let taken = fx.next("bob").expect("bob takes the item");
    assert_eq!(taken.request_id, rid);
    fx.act("bob", &rid, RequestAction::Complete, None).unwrap();

    let (out, _) = fx.store.outbound_events(0, 10_000).unwrap();
    let leaked: Vec<String> = out
        .iter()
        .filter(|e| e.payload.contains("group_id") || e.payload.contains("secret migration"))
        .map(|e| format!("{} {}", e.kind, e.payload))
        .collect();
    assert!(
        leaked.is_empty(),
        "queue item events offered to peers:\n{}",
        leaked.join("\n")
    );
}

/// Adversarial probe (review of #152): an agent outside the group, who neither sent nor
/// owns a queue item, may not release, complete or cancel it.
#[test]
fn adv152_outsiders_cannot_act_on_a_queue_item() {
    let fx = Fx::new();
    fx.reviewers();
    let rid = fx.queue("Review", Priority::Normal, None);
    for action in [
        RequestAction::Release,
        RequestAction::Complete,
        RequestAction::Cancel,
    ] {
        assert!(
            fx.act("carol", &rid, action, None).is_err(),
            "carol could {action:?} an open queue item"
        );
    }
    fx.next("bob").unwrap();
    for action in [
        RequestAction::Release,
        RequestAction::Complete,
        RequestAction::Cancel,
    ] {
        assert!(
            fx.act("carol", &rid, action, None).is_err(),
            "carol could {action:?} bob's queue item"
        );
    }
}
