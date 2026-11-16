//! Who may complete a request, and who hears about it. Claimed work belongs to its owner;
//! stale work may be taken over by any local agent (completing it is a takeover); open work
//! may be completed by any local agent. A token caller is further limited to requests it
//! created, received or owns (see `request_scope.rs`). The creator is told "Done: <title>"
//! unless it completed the request itself.

use std::sync::atomic::Ordering;

use another_core::*;

const T0: i64 = 1_800_000_000_000;

struct Fx {
    _dir: tempfile::TempDir,
    store: Store,
    now: std::sync::Arc<std::sync::atomic::AtomicI64>,
}

fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let (clock, now) = Clock::manual(T0);
    let store = Store::open_with_clock(&dir.path().join("node"), clock).unwrap();
    for name in ["lead", "worker", "other"] {
        store
            .register_agent(RegisterAgent {
                name: name.into(),
                program: Program::Other,
                project: None,
                model: None,
                description: None,
            })
            .unwrap();
    }
    Fx {
        _dir: dir,
        store,
        now,
    }
}

impl Fx {
    /// lead asks worker and other.
    fn request(&self) -> String {
        self.store
            .send_message(SendMessage {
                from: "lead".into(),
                to: vec!["worker".into(), "other".into()],
                subject: "Ship it".into(),
                body: "b".into(),
                request: Some(NewRequest {
                    due_in_seconds: None,
                }),
                ..Default::default()
            })
            .unwrap()
            .request_id
            .unwrap()
    }

    fn act(&self, who: &str, id: &str, action: RequestAction, due: Option<i64>) -> Result<Request> {
        self.store.update_request(RequestUpdate {
            agent: who.into(),
            request_id: id.into(),
            action,
            note: None,
            due_in_seconds: due,
        })
    }

    /// "Done: …" messages in lead's inbox.
    fn dones(&self) -> usize {
        self.store
            .inbox(
                "lead",
                InboxQuery {
                    unread_only: false,
                    urgent_only: false,
                    limit: 100,
                },
            )
            .unwrap()
            .messages
            .iter()
            .filter(|m| m.message.subject == "Done: Ship it")
            .count()
    }
}

#[test]
fn only_the_owner_completes_claimed_work() {
    let f = fx();
    let id = f.request();
    f.act("worker", &id, RequestAction::Claim, None).unwrap();

    let err = f
        .act("other", &id, RequestAction::Complete, None)
        .unwrap_err();
    assert_eq!(err.code(), "conflict", "{err}");
    assert!(err.to_string().contains("claimed by worker"), "{err}");
    assert!(
        err.to_string().contains("only the owner can complete it"),
        "{err}"
    );
    let r = f.store.request(&id).unwrap();
    assert_eq!(r.status, RequestStatus::Claimed);
    assert_eq!(r.owner.as_deref(), Some("worker"));
    assert_eq!(f.dones(), 0, "nothing was completed");

    let done = f.act("worker", &id, RequestAction::Complete, None).unwrap();
    assert_eq!(done.status, RequestStatus::Done);
    assert_eq!(f.dones(), 1);
}

#[test]
fn completing_stale_work_takes_it_over() {
    let f = fx();
    let id = f.request();
    f.act("worker", &id, RequestAction::Claim, Some(60))
        .unwrap();
    f.now.fetch_add(120_000, Ordering::SeqCst);
    f.store.sweep().unwrap();
    assert_eq!(f.store.request(&id).unwrap().status, RequestStatus::Stale);

    let done = f
        .act("other", &id, RequestAction::Complete, None)
        .expect("stale work may be taken over");
    assert_eq!(done.status, RequestStatus::Done);
    assert_eq!(done.owner.as_deref(), Some("other"));
    assert_eq!(f.dones(), 1);
}

#[test]
fn anyone_may_complete_open_work_and_becomes_its_owner() {
    let f = fx();
    let id = f.request();
    let done = f.act("other", &id, RequestAction::Complete, None).unwrap();
    assert_eq!(done.status, RequestStatus::Done);
    assert_eq!(done.owner.as_deref(), Some("other"));
    assert_eq!(f.dones(), 1);
}

#[test]
fn a_creator_completing_its_own_request_is_not_told_about_it() {
    let f = fx();
    let id = f.request();
    let done = f.act("lead", &id, RequestAction::Complete, None).unwrap();
    assert_eq!(done.status, RequestStatus::Done);
    assert_eq!(f.dones(), 0);
}

/// adv216: completing open work without a note tells the creator "Request <id> is done.",
/// a blank note counts as none, and the request records when it was taken (its
/// `claimed_at`), which a direct completion sets to the completion time.
#[test]
fn adv216_a_direct_completion_without_a_note_gets_the_default_body_and_a_claim_time() {
    let f = fx();
    for note in [None, Some("   ".to_string())] {
        let id = f.request();
        let done = f
            .store
            .update_request(RequestUpdate {
                agent: "other".into(),
                request_id: id.clone(),
                action: RequestAction::Complete,
                note,
                due_in_seconds: None,
            })
            .unwrap();
        assert!(done.closed_at.is_some());
        assert_eq!(done.claimed_at, done.closed_at, "{done:?}");
        let inbox = f
            .store
            .inbox(
                "lead",
                InboxQuery {
                    unread_only: false,
                    urgent_only: false,
                    limit: 100,
                },
            )
            .unwrap();
        let body = &inbox
            .messages
            .iter()
            .find(|m| m.message.subject == "Done: Ship it" && m.message.body.contains(&id))
            .unwrap_or_else(|| panic!("no Done mail naming {id}"))
            .message
            .body;
        assert_eq!(body, &format!("Request {id} is done."));
    }
}

/// adv216-b: completing claimed work keeps the time it was claimed, so the record still shows
/// how long the work was held.
#[test]
fn adv216_b_completing_claimed_work_keeps_its_claim_time() {
    let f = fx();
    let id = f.request();
    let claimed_at = f
        .act("worker", &id, RequestAction::Claim, None)
        .unwrap()
        .claimed_at;
    assert!(claimed_at.is_some());
    f.now.fetch_add(60_000, Ordering::SeqCst);
    let done = f.act("worker", &id, RequestAction::Complete, None).unwrap();
    assert_eq!(done.claimed_at, claimed_at, "{done:?}");
    assert_ne!(done.claimed_at, done.closed_at, "{done:?}");
}
