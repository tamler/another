//! Lines the activity feed and the catch-up digest give agents that no test read: a request
//! released or cancelled, a token created or revoked, a node paired; the summary refusing a
//! cursor it does not know; and `whats_new` defaulting and capping its limit and naming
//! urgent mail in its next step.

use another_core::*;

fn store(dir: &std::path::Path) -> Store {
    let s = Store::open(&dir.join("home")).unwrap();
    for name in ["lead", "worker"] {
        s.register_agent(RegisterAgent {
            name: name.into(),
            program: Program::Other,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    }
    s
}

fn lines(s: &Store) -> Vec<String> {
    s.activity(ActivityQuery {
        since: Some(Since::Time(0)),
        limit: 500,
        kinds: vec![],
    })
    .unwrap()
    .items
    .into_iter()
    .map(|i| i.summary)
    .collect()
}

fn has(lines: &[String], text: &str) -> bool {
    lines.iter().any(|l| l.contains(text))
}

fn request(s: &Store, subject: &str) -> String {
    s.send_message(SendMessage {
        from: "lead".into(),
        to: vec!["worker".into()],
        subject: subject.into(),
        body: "b".into(),
        request: Some(NewRequest::default()),
        ..Default::default()
    })
    .unwrap()
    .request_id
    .unwrap()
}

fn act(s: &Store, who: &str, id: &str, action: RequestAction) {
    s.update_request(RequestUpdate {
        agent: who.into(),
        request_id: id.into(),
        action,
        note: None,
        due_in_seconds: None,
    })
    .unwrap();
}

#[test]
fn request_token_and_pairing_events_read_as_one_line_each() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let released = request(&s, "Port the parser");
    act(&s, "worker", &released, RequestAction::Claim);
    act(&s, "worker", &released, RequestAction::Release);
    let cancelled = request(&s, "Old idea");
    act(&s, "lead", &cancelled, RequestAction::Cancel);
    let token = s.create_token("lead", &[], None, None).unwrap();
    s.revoke_token(&token.token.token_id).unwrap();

    let hub = store(&dir.path().join("hub"));
    hub.set_node_name("hub").unwrap();
    s.set_node_name("alpha").unwrap();
    test_support::pair(&hub, &s, &Perm::ALL);
    hub.revoke_peer(s.node_id()).unwrap();

    let feed = lines(&s);
    let id = &token.token.token_id;
    for want in [
        "released request: Port the parser".to_string(),
        "cancelled request: Old idea".to_string(),
        format!("remote access token {id} created"),
        format!("remote access token {id} revoked"),
        "paired with node hub (hub)".to_string(),
    ] {
        assert!(has(&feed, &want), "missing {want:?} in {feed:#?}");
    }
    let hub_feed = lines(&hub);
    assert!(
        has(&hub_feed, "paired with node alpha (spoke)"),
        "{hub_feed:#?}"
    );
    assert!(
        has(&hub_feed, &format!("unpaired node {}", s.node_id())),
        "{hub_feed:#?}"
    );
}

#[test]
fn a_summary_from_an_unknown_event_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    let err = s
        .activity_summary(Some(Since::After("01ARZ3NDEKTSV4RRFFQ69G5FAV".into())))
        .unwrap_err();
    assert_eq!(err.code(), "not_found", "{err}");
    assert!(err.to_string().contains("unknown event id"), "{err}");

    // A known event works as the start of the summary, and counts what came after it.
    let first = s
        .activity(ActivityQuery {
            since: Some(Since::Time(0)),
            limit: 1,
            kinds: vec![],
        })
        .unwrap()
        .items
        .remove(0)
        .event_id;
    let sum = s.activity_summary(Some(Since::After(first))).unwrap();
    assert!(sum.text.contains("event(s) since"), "{}", sum.text);
}

#[test]
fn whats_new_defaults_and_caps_its_limit_and_names_urgent_mail() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    for n in 0..25 {
        s.send_message(SendMessage {
            from: "lead".into(),
            to: vec!["worker".into()],
            subject: format!("note {n}"),
            body: "b".into(),
            importance: if n == 0 {
                Importance::Urgent
            } else {
                Importance::Normal
            },
            ..Default::default()
        })
        .unwrap();
    }
    let default = s.whats_new("worker", None, 0).unwrap();
    assert_eq!(default.messages.len(), 10, "limit 0 means 10");
    let capped = s.whats_new("worker", None, 1000).unwrap();
    assert_eq!(capped.messages.len(), 20, "the limit is capped at 20");
    assert_eq!(capped.unread, 25);
    assert_eq!(capped.urgent_unread, 1);
    assert!(
        capped.next.contains("25 unread message(s), 1 urgent"),
        "{}",
        capped.next
    );

    let idle = s.whats_new("lead", None, 0).unwrap();
    assert!(
        idle.next.starts_with("Nothing is waiting for lead"),
        "{}",
        idle.next
    );
}

/// adv230: a small `whats_new` limit is kept (not raised to the default) and bounds the
/// feed as well as the messages; request lines name who acted.
#[test]
fn adv230_a_small_limit_bounds_messages_and_feed_and_lines_name_the_actor() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path());
    for n in 0..6 {
        s.send_message(SendMessage {
            from: "lead".into(),
            to: vec!["worker".into()],
            subject: format!("note {n}"),
            body: "b".into(),
            ..Default::default()
        })
        .unwrap();
    }
    let small = s.whats_new("worker", None, 3).unwrap();
    assert_eq!(small.messages.len(), 3, "a limit of 3 is kept");
    assert!(
        small.activity.len() <= 3,
        "the activity list ignores the limit: {}",
        small.activity.len()
    );

    let id = request(&s, "Port the parser");
    act(&s, "worker", &id, RequestAction::Claim);
    act(&s, "worker", &id, RequestAction::Release);
    assert!(
        has(&lines(&s), "worker released request: Port the parser"),
        "{:#?}",
        lines(&s)
    );
}
