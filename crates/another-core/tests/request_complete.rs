//! Who may complete a request, and who hears about it. Claimed work belongs to its owner;
//! stale work may be taken over by anyone (completing it is a takeover); open work may be
//! completed by anyone. The creator is told "Done: <title>" unless it completed the request
//! itself.

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
