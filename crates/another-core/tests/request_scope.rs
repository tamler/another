//! `Store::update_request_scoped`, the path every remote `claim_request` takes: a token's
//! agent may change only a request it created, received (to or cc) or owns. Any other
//! request must look exactly like an unknown id, whatever its status, so a token cannot
//! probe for requests it was never part of.

use std::sync::atomic::Ordering;

use another_core::*;

const T0: i64 = 1_800_000_000_000;

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    now: std::sync::Arc<std::sync::atomic::AtomicI64>,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let (clock, now) = Clock::manual(T0);
    let store = Store::open_with_clock(&dir.path().join("node"), clock).unwrap();
    for name in ["alice", "bob", "carol", "dave", "erin"] {
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
    Fixture {
        _dir: dir,
        store,
        now,
    }
}

impl Fixture {
    fn scope(&self, agent: &str) -> TokenScope {
        let new = self
            .store
            .create_token(agent, &["**".to_string()], None, None)
            .unwrap();
        self.store.verify_token(&new.secret).unwrap()
    }

    /// alice asks bob, cc erin; the request is due in 60 s.
    fn request(&self) -> String {
        self.store
            .send_message(SendMessage {
                from: "alice".into(),
                to: vec!["bob".into()],
                cc: vec!["erin".into()],
                subject: "please do it".into(),
                body: "b".into(),
                request: Some(NewRequest {
                    due_in_seconds: Some(60),
                }),
                ..Default::default()
            })
            .unwrap()
            .request_id
            .expect("the message opened a request")
    }

    fn update(
        &self,
        scope: Option<&TokenScope>,
        agent: &str,
        request_id: &str,
        action: RequestAction,
    ) -> Result<Request> {
        self.store.update_request_scoped(
            RequestUpdate {
                agent: agent.into(),
                request_id: request_id.into(),
                action,
                note: None,
                due_in_seconds: None,
            },
            scope,
        )
    }
}

/// The error for a request outside the scope, with its id swapped for a placeholder, so it
/// can be compared with the error for an id that does not exist.
fn hidden(err: &Error, id: &str) -> (String, String) {
    (err.code().to_string(), err.to_string().replace(id, "<id>"))
}

#[test]
fn an_outsider_token_sees_an_existing_request_as_unknown() {
    let f = fixture();
    let rid = f.request();
    let carol = f.scope("carol");

    let missing = "01NOSUCHREQUESTNOSUCHREQUE";
    let err_missing = f
        .update(Some(&carol), "carol", missing, RequestAction::Claim)
        .unwrap_err();
    assert_eq!(err_missing.code(), "not_found", "{err_missing}");

    for action in [
        RequestAction::Claim,
        RequestAction::Release,
        RequestAction::Complete,
        RequestAction::Cancel,
    ] {
        let err = f.update(Some(&carol), "carol", &rid, action).unwrap_err();
        assert_eq!(hidden(&err, &rid), hidden(&err_missing, missing));
    }
    // Nothing changed: bob can still claim it.
    let bob = f.scope("bob");
    let claimed = f
        .update(Some(&bob), "bob", &rid, RequestAction::Claim)
        .unwrap();
    assert_eq!(claimed.status, RequestStatus::Claimed);
}

/// Creator, `to` recipient and `cc` recipient are all inside the scope.
#[test]
fn creator_and_recipients_may_act_through_a_token() {
    let f = fixture();

    let rid = f.request();
    let erin = f.scope("erin");
    let r = f
        .update(Some(&erin), "erin", &rid, RequestAction::Claim)
        .expect("a cc recipient may claim");
    assert_eq!(r.status, RequestStatus::Claimed);
    let r = f
        .update(Some(&erin), "erin", &rid, RequestAction::Release)
        .unwrap();
    assert_eq!(r.status, RequestStatus::Open);

    let bob = f.scope("bob");
    f.update(Some(&bob), "bob", &rid, RequestAction::Claim)
        .expect("the recipient may claim");
    f.update(Some(&bob), "bob", &rid, RequestAction::Complete)
        .expect("the owner may complete");

    let rid = f.request();
    let alice = f.scope("alice");
    let r = f
        .update(Some(&alice), "alice", &rid, RequestAction::Cancel)
        .expect("the creator may cancel");
    assert_eq!(r.status, RequestStatus::Cancelled);
}

/// A stale request may be taken over by anyone locally; the new owner is then inside the
/// scope even though it neither created nor received the request.
#[test]
fn an_owner_who_took_over_stale_work_may_act_through_a_token() {
    let f = fixture();
    let rid = f.request();
    f.now.fetch_add(120_000, Ordering::SeqCst);
    let dave = f.scope("dave");
    // Before taking it over, dave is an outsider.
    assert_eq!(
        f.update(Some(&dave), "dave", &rid, RequestAction::Claim)
            .unwrap_err()
            .code(),
        "not_found"
    );
    let r = f
        .update(None, "dave", &rid, RequestAction::Claim)
        .expect("anyone may take over stale work locally");
    assert_eq!(r.owner.as_deref(), Some("dave"));
    let r = f
        .update(Some(&dave), "dave", &rid, RequestAction::Complete)
        .expect("the owner may complete through a token");
    assert_eq!(r.status, RequestStatus::Done);
}

/// A closed request outside the scope is still just unknown: the token learns neither that
/// it exists nor that it is done or cancelled. Inside the scope the real conflict shows.
#[test]
fn a_closed_request_outside_the_scope_does_not_reveal_its_status() {
    let f = fixture();
    let rid = f.request();
    f.update(None, "alice", &rid, RequestAction::Cancel)
        .unwrap();

    let carol = f.scope("carol");
    let err = f
        .update(Some(&carol), "carol", &rid, RequestAction::Claim)
        .unwrap_err();
    assert_eq!(err.code(), "not_found", "{err}");
    assert!(!err.to_string().contains("cancelled"), "{err}");

    let bob = f.scope("bob");
    let err = f
        .update(Some(&bob), "bob", &rid, RequestAction::Claim)
        .unwrap_err();
    assert_eq!(err.code(), "conflict", "{err}");
}
