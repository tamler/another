//! Requests and their audit trail: deadline moves and release (A1, A2 of the adversarial
//! pass on `main` `6891b13`). `control_*` tests pin behaviour that should stay.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use another_core::*;

const T0: i64 = 1_800_000_000_000;

struct Fx {
    _dir: tempfile::TempDir,
    store: Store,
    clock: Arc<AtomicI64>,
}

impl Fx {
    fn new() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work/app").to_string_lossy().into_owned();
        let (clock, handle) = Clock::manual(T0);
        let store = Store::open_in_memory(clock).unwrap().unguarded_repair();
        store.ensure_project("test", &path).unwrap();
        for name in ["lead", "worker", "reviewer"] {
            store
                .register_agent(RegisterAgent {
                    name: name.into(),
                    program: Program::Claude,
                    project: None,
                    model: None,
                    description: None,
                })
                .unwrap();
        }
        Fx {
            _dir: dir,
            store,
            clock: handle,
        }
    }

    fn advance_secs(&self, secs: i64) {
        self.clock.fetch_add(secs * 1000, Ordering::SeqCst);
    }

    /// `lead` opens a request to `worker`, due in `due` seconds.
    fn open(&self, due: Option<i64>) -> String {
        self.store
            .send_message(SendMessage {
                from: "lead".into(),
                to: vec!["worker".into()],
                subject: "Fix the flaky test".into(),
                body: "tests/net.rs".into(),
                request: Some(NewRequest {
                    due_in_seconds: due,
                }),
                ..Default::default()
            })
            .unwrap()
            .request_id
            .expect("request opened")
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

    fn request_events(&self, rid: &str) -> Vec<(String, serde_json::Value)> {
        self.store
            .events(10_000)
            .unwrap()
            .into_iter()
            .filter(|e| e.payload.get("request_id").and_then(|v| v.as_str()) == Some(rid))
            .map(|e| (e.kind, e.payload))
            .collect()
    }
}

/// A1. The owner moving its own deadline changes `requests.due_at` but writes no event, so
/// the event log (the audit trail, and the activity feed built from it) cannot show when or
/// by whom the deadline moved. Every other request transition is an event.
#[test]
fn an_owner_moving_its_deadline_is_recorded_as_an_event() {
    let fx = Fx::new();
    let rid = fx.open(Some(600));
    fx.act("worker", &rid, RequestAction::Claim, None).unwrap();
    let before = fx.request_events(&rid);
    let due_before = fx.store.request(&rid).unwrap().due_at;

    let after_reclaim = fx
        .act("worker", &rid, RequestAction::Claim, Some(86_400))
        .unwrap();
    assert_ne!(after_reclaim.due_at, due_before, "the deadline did move");

    let after = fx.request_events(&rid);
    assert!(
        after.len() > before.len(),
        "moving the deadline from {due_before:?} to {:?} left no event; events stayed {:?}",
        after_reclaim.due_at,
        after.iter().map(|(k, _)| k).collect::<Vec<_>>()
    );
    let (kind, payload) = after.last().unwrap();
    assert_eq!(kind, "request.updated");
    assert_eq!(payload["due_at"].as_i64().map(fmt_ts), after_reclaim.due_at);
    assert_eq!(payload["previous_due_at"].as_i64().map(fmt_ts), due_before);
    // It is not a second claim: `another stats` counts claims from request.claimed.
    let claims = after.iter().filter(|(k, _)| k == "request.claimed").count();
    assert_eq!(claims, 1);
    // The activity feed shows it.
    let feed = fx
        .store
        .activity(ActivityQuery {
            since: None,
            limit: 50,
            kinds: vec![],
        })
        .unwrap();
    assert!(
        feed.items.iter().any(|i| i
            .summary
            .contains("moved the deadline of request: Fix the flaky test")),
        "{:?}",
        feed.items.iter().map(|i| &i.summary).collect::<Vec<_>>()
    );
}

/// A2. An owner who releases a stale request gets `open`, but the old, already-past due
/// time is kept, so the next sweep turns it `stale` again with no owner and no action by
/// anyone. REQUESTS.md says release "returns the request to open with no owner, so anyone
/// can claim it".
#[test]
fn a_released_stale_request_does_not_go_stale_again_by_itself() {
    let fx = Fx::new();
    let rid = fx.open(Some(60));
    fx.act("worker", &rid, RequestAction::Claim, None).unwrap();
    fx.advance_secs(120);
    fx.store.sweep().unwrap();
    assert_eq!(fx.store.request(&rid).unwrap().status, RequestStatus::Stale);

    let released = fx
        .act("worker", &rid, RequestAction::Release, None)
        .unwrap();
    assert_eq!(released.status, RequestStatus::Open);
    fx.advance_secs(1);
    fx.store.sweep().unwrap();
    let now = fx.store.request(&rid).unwrap();
    assert_eq!(
        now.status,
        RequestStatus::Open,
        "released at {:?}, it went {:?} again one second later with due_at {:?}",
        released.updated_at,
        now.status,
        now.due_at
    );
}

/// Control: takeover of stale work clears the past due time (REQUESTS §5c).
#[test]
fn control_a_takeover_without_due_clears_the_past_deadline() {
    let fx = Fx::new();
    let rid = fx.open(Some(60));
    fx.act("worker", &rid, RequestAction::Claim, None).unwrap();
    fx.advance_secs(120);
    fx.store.sweep().unwrap();
    let taken = fx
        .act("reviewer", &rid, RequestAction::Claim, None)
        .unwrap();
    assert_eq!(taken.status, RequestStatus::Claimed);
    assert_eq!(taken.due_at, None);
    let claimed = fx
        .request_events(&rid)
        .into_iter()
        .rev()
        .find(|(k, _)| k == "request.claimed")
        .unwrap()
        .1;
    assert!(claimed
        .get("previous_owner_id")
        .is_some_and(|v| !v.is_null()));
}

/// Control: closed requests refuse every action, including cancel after done.
#[test]
fn control_closed_requests_refuse_every_action() {
    let fx = Fx::new();
    let rid = fx.open(None);
    fx.act("worker", &rid, RequestAction::Claim, None).unwrap();
    fx.act("worker", &rid, RequestAction::Complete, None)
        .unwrap();
    for (agent, action) in [
        ("lead", RequestAction::Cancel),
        ("reviewer", RequestAction::Claim),
        ("worker", RequestAction::Release),
        ("worker", RequestAction::Complete),
    ] {
        let err = fx.act(agent, &rid, action, None).unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "{err:?}");
    }
}

/// Control: absurd due times are refused, not wrapped.
#[test]
fn control_absurd_due_times_are_errors() {
    let fx = Fx::new();
    let rid = fx.open(None);
    for due in [0, -5, i64::MAX] {
        let err = fx.act("worker", &rid, RequestAction::Claim, Some(due));
        assert!(err.is_err(), "due {due} was accepted: {err:?}");
    }
}
