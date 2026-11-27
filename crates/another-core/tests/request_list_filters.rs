//! Listing requests: the owner filter (by name or `name@project`), the status filter, the
//! limit with newest first, and the status and action names the CLI and MCP parse.

use std::sync::atomic::Ordering;

use another_core::*;

const T0: i64 = 1_800_000_000_000;

fn register(store: &Store, name: &str, project: Option<&str>) {
    store
        .register_agent(RegisterAgent {
            name: name.into(),
            program: Program::Claude,
            project: project.map(Into::into),
            model: None,
            description: None,
        })
        .unwrap();
}

fn titles(list: &[Request]) -> Vec<String> {
    list.iter().map(|r| r.title.clone()).collect()
}

#[test]
fn requests_filter_by_owner_and_status_newest_first_within_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let shop = dir.path().join("work").join("shop");
    let shop = shop.to_string_lossy().into_owned();
    let (clock, now) = Clock::manual(T0);
    let s = Store::open_in_memory(clock).unwrap();
    register(&s, "alice", None);
    register(&s, "bob", None);
    register(&s, "carol", Some(&shop));

    // Five requests a minute apart, from alice to bob and carol@shop.
    let mut rid = Vec::new();
    for n in 1..=5 {
        now.fetch_add(60_000, Ordering::SeqCst);
        let msg = s
            .send_message(SendMessage {
                from: "alice".into(),
                to: vec!["bob".into(), "carol@shop".into()],
                subject: format!("r{n}"),
                body: String::new(),
                request: Some(NewRequest::default()),
                ..Default::default()
            })
            .unwrap();
        rid.push(msg.request_id.unwrap());
    }
    let act = |agent: &str, i: usize, action: RequestAction| {
        s.update_request(RequestUpdate {
            agent: agent.into(),
            request_id: rid[i].clone(),
            action,
            note: None,
            due_in_seconds: None,
        })
        .unwrap()
    };
    act("bob", 0, RequestAction::Claim);
    act("carol@shop", 1, RequestAction::Claim);
    act("carol@shop", 2, RequestAction::Claim);
    act("carol@shop", 2, RequestAction::Complete);
    act("alice", 3, RequestAction::Cancel);

    let list = |status: Option<RequestStatus>, owner: Option<&str>, limit: u32| {
        s.requests(RequestFilter {
            status,
            owner: owner.map(Into::into),
            limit,
        })
        .unwrap()
    };

    // No filter: all five, newest first.
    assert_eq!(titles(&list(None, None, 0)), ["r5", "r4", "r3", "r2", "r1"]);
    // The limit keeps the newest.
    assert_eq!(titles(&list(None, None, 2)), ["r5", "r4"]);

    // Owner by plain name and by name@project; each sees only what it owns.
    assert_eq!(titles(&list(None, Some("bob"), 0)), ["r1"]);
    assert_eq!(titles(&list(None, Some("carol@shop"), 0)), ["r3", "r2"]);
    assert_eq!(
        titles(&list(Some(RequestStatus::Claimed), Some("carol@shop"), 0)),
        ["r2"]
    );
    assert!(list(Some(RequestStatus::Done), Some("bob"), 0).is_empty());
    // An agent that owns nothing gets an empty list, not every request.
    assert!(list(None, Some("alice"), 0).is_empty());

    // Status alone.
    assert_eq!(titles(&list(Some(RequestStatus::Open), None, 0)), ["r5"]);
    assert_eq!(
        titles(&list(Some(RequestStatus::Claimed), None, 0)),
        ["r2", "r1"]
    );
    assert_eq!(titles(&list(Some(RequestStatus::Done), None, 0)), ["r3"]);
    assert_eq!(
        titles(&list(Some(RequestStatus::Cancelled), None, 0)),
        ["r4"]
    );
    for r in list(Some(RequestStatus::Claimed), None, 0) {
        assert_eq!(r.status, RequestStatus::Claimed);
    }

    // An owner that does not resolve is an error, never an unfiltered list.
    let err = s
        .requests(RequestFilter {
            owner: Some("nobody".into()),
            ..Default::default()
        })
        .unwrap_err();
    assert_eq!(err.code(), "not_found", "{err}");
}

#[test]
fn status_and_action_names_round_trip_and_unknown_names_are_refused() {
    for status in [
        RequestStatus::Open,
        RequestStatus::Claimed,
        RequestStatus::Done,
        RequestStatus::Stale,
        RequestStatus::Cancelled,
    ] {
        assert_eq!(status.as_str().parse::<RequestStatus>().unwrap(), status);
    }
    for (name, action) in [
        ("claim", RequestAction::Claim),
        ("release", RequestAction::Release),
        ("complete", RequestAction::Complete),
        ("cancel", RequestAction::Cancel),
    ] {
        assert_eq!(name.parse::<RequestAction>().unwrap(), action);
    }
    for bad in ["", "Open", "canceled", "closed"] {
        let err = bad.parse::<RequestStatus>().unwrap_err();
        assert_eq!(err.code(), "invalid", "{bad:?}");
        assert!(err.to_string().contains("expected open, claimed"), "{err}");
    }
    for bad in ["", "Claim", "done", "reopen"] {
        let err = bad.parse::<RequestAction>().unwrap_err();
        assert_eq!(err.code(), "invalid", "{bad:?}");
        assert!(err.to_string().contains("expected claim, release"), "{err}");
    }
}
