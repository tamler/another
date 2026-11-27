//! The session-start digest's lines: unread messages carry an `[urgent]` or `[high]` tag
//! (and nothing for normal or low), requests show their status and owner, and message
//! bodies never appear. From pull request 245, in the line shapes of D96; how stored text
//! is cleaned and quoted is tested in hooks_queue.rs.

use another::hooks::Client;
use another_core::*;

fn session_digest(store: &Store, agent: &str) -> anyhow::Result<String> {
    another::hooks::session_digest(store, agent, Client::Claude, &[])
}

fn store() -> Store {
    let s = Store::open_in_memory(Clock::manual(1_800_000_000_000).0).unwrap();
    for name in ["alice", "bob"] {
        s.register_agent(RegisterAgent {
            name: name.into(),
            program: Program::Claude,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    }
    s
}

fn send(s: &Store, subject: &str, importance: Importance, request: bool) -> Message {
    s.send_message(SendMessage {
        from: "alice".into(),
        to: vec!["bob".into()],
        subject: subject.into(),
        body: format!("secret body of {subject}"),
        importance,
        request: request.then(NewRequest::default),
        ..Default::default()
    })
    .unwrap()
}

fn line_with<'a>(digest: &'a str, needle: &str) -> &'a str {
    digest
        .lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no line with {needle:?} in:\n{digest}"))
}

#[test]
fn unread_lines_are_tagged_by_importance_and_never_show_bodies() {
    let s = store();
    let low = send(&s, "low note", Importance::Low, false);
    let normal = send(&s, "normal note", Importance::Normal, false);
    let high = send(&s, "high note", Importance::High, false);
    let urgent = send(&s, "urgent note", Importance::Urgent, false);

    let digest = session_digest(&s, "bob").unwrap();
    assert!(
        digest.starts_with(
            "Another: bob has 4 unread (2 urgent), 0 awaiting ack, 0 open request(s)."
        ),
        "{digest}"
    );
    assert_eq!(
        line_with(&digest, "urgent note"),
        format!(
            "- [urgent] alice, message {}: \"urgent note\"",
            urgent.message_id
        )
    );
    assert_eq!(
        line_with(&digest, "high note"),
        format!("- [high] alice, message {}: \"high note\"", high.message_id)
    );
    assert_eq!(
        line_with(&digest, "normal note"),
        format!("- alice, message {}: \"normal note\"", normal.message_id)
    );
    assert_eq!(
        line_with(&digest, "low note"),
        format!("- alice, message {}: \"low note\"", low.message_id)
    );
    assert!(!digest.contains("secret body"), "{digest}");
}

#[test]
fn request_lines_show_status_and_owner_only_when_there_is_one() {
    let s = store();
    let open = send(&s, "open task", Importance::Normal, true);
    let taken = send(&s, "taken task", Importance::Normal, true);
    let open_id = open.request_id.unwrap();
    let taken_id = taken.request_id.unwrap();
    s.update_request(RequestUpdate {
        agent: "bob".into(),
        request_id: taken_id.clone(),
        action: RequestAction::Claim,
        note: None,
        due_in_seconds: None,
    })
    .unwrap();

    let digest = session_digest(&s, "bob").unwrap();
    assert!(digest.contains("2 open request(s)"), "{digest}");
    assert_eq!(
        line_with(&digest, &format!("request {open_id}")),
        format!("- request {open_id} from alice (open): \"open task\"")
    );
    assert_eq!(
        line_with(&digest, &format!("request {taken_id}")),
        format!("- request {taken_id} from alice (claimed, owner bob): \"taken task\"")
    );
}

/// adv245: the header counts every open request, but at most five request lines follow, so a
/// long list of requests cannot fill the capped digest.
#[test]
fn adv245_at_most_five_request_lines_follow_the_full_count() {
    let s = store();
    for n in 0..7 {
        send(&s, &format!("task {n}"), Importance::Normal, true);
    }
    let digest = session_digest(&s, "bob").unwrap();
    assert!(digest.contains("7 open request(s)"), "{digest}");
    let lines = digest
        .lines()
        .filter(|l| l.starts_with("- request "))
        .count();
    assert_eq!(lines, 5, "{digest}");
}
