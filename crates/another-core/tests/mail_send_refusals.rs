//! What `send_message` and `acknowledge` refuse before anything is stored: an empty
//! subject, a subject that is not one safe line (it is typed into terminals by wakes), no
//! recipients, a request opened with an agent on another node, and an acknowledgement of no
//! messages. Each refusal stores nothing.

use another_core::*;

fn store(dir: &std::path::Path, name: &str) -> Store {
    let s = Store::open(&dir.join(name)).unwrap();
    s.set_node_name(name).unwrap();
    for agent in ["alice", "bob"] {
        s.register_agent(RegisterAgent {
            name: agent.into(),
            program: Program::Claude,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    }
    s
}

fn mail(subject: &str, to: &[&str]) -> SendMessage {
    SendMessage {
        from: "alice".into(),
        to: to.iter().map(|t| t.to_string()).collect(),
        subject: subject.into(),
        body: "b".into(),
        ..Default::default()
    }
}

fn bobs_mail(s: &Store) -> usize {
    s.inbox(
        "bob",
        InboxQuery {
            unread_only: false,
            urgent_only: false,
            limit: 100,
        },
    )
    .unwrap()
    .messages
    .len()
}

#[test]
fn a_subject_must_be_one_nonempty_safe_line() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), "node");
    for (subject, why) in [
        ("", "empty"),
        ("   ", "blank"),
        ("a\nb", "line feed"),
        ("a\rb", "carriage return"),
        ("a\tb", "tab"),
        ("\u{1b}[31mred", "ESC"),
        ("a\u{0}b", "NUL"),
        ("a\u{7f}b", "DEL"),
        ("a\u{85}b", "NEL (C1)"),
        ("a\u{9b}31mb", "CSI (C1)"),
        ("abc\u{202e}fed", "right-to-left override"),
        ("a\u{2066}b\u{2069}", "left-to-right isolate"),
    ] {
        let err = s.send_message(mail(subject, &["bob"])).unwrap_err();
        assert_eq!(err.code(), "invalid", "{why}: {err}");
    }
    assert_eq!(bobs_mail(&s), 0, "a refused send stored a message");
    // Ordinary text, including non-ASCII and punctuation, is fine.
    s.send_message(mail("café: fix «build» — 100% ✓", &["bob"]))
        .unwrap();
    assert_eq!(bobs_mail(&s), 1);
}

#[test]
fn a_message_needs_a_recipient_and_an_ack_needs_a_message() {
    let dir = tempfile::tempdir().unwrap();
    let s = store(dir.path(), "node");
    let err = s.send_message(mail("hello", &[])).unwrap_err();
    assert_eq!(err.code(), "invalid", "{err}");
    assert!(err.to_string().contains("at least one recipient"), "{err}");
    for ack in [false, true] {
        let err = s.acknowledge("bob", &[], ack).unwrap_err();
        assert_eq!(err.code(), "invalid", "{err}");
        assert!(err.to_string().contains("no message ids"), "{err}");
    }
}

/// Mail may cross to a paired node; a request may not, and the refusal stores nothing.
#[test]
fn a_request_cannot_be_opened_with_an_agent_on_another_node() {
    let dir = tempfile::tempdir().unwrap();
    let hub = store(dir.path(), "hub");
    let spoke = store(dir.path(), "spoke");
    let code = hub
        .create_invite("https://hub.example", &Perm::ALL, 3600)
        .unwrap();
    let (invite, req) = spoke.prepare_join(&code).unwrap();
    let resp = hub.accept_pairing(&req).unwrap();
    spoke.complete_join(&invite, &resp).unwrap();
    let (events, _) = hub.outbound_events(0, 10_000).unwrap();
    let pull = spoke
        .ingest(hub.node_id(), &events, &[hub.node_entry().unwrap()])
        .unwrap();
    assert!(pull.rejected.is_empty(), "{pull:?}");

    let mut ask = mail("please review", &["bob@@hub"]);
    ask.request = Some(NewRequest {
        due_in_seconds: None,
    });
    let err = spoke.send_message(ask).unwrap_err();
    assert_eq!(err.code(), "invalid", "{err}");
    assert!(err.to_string().contains("requests are node-local"), "{err}");
    let (outbound, _) = spoke.outbound_events(0, 10_000).unwrap();
    assert!(
        outbound.iter().all(|e| e.kind != "message.sent"),
        "a refused request left mail to sync"
    );

    // The same mail without a request goes.
    let sent = spoke
        .send_message(mail("please review", &["bob@@hub"]))
        .unwrap();
    assert_eq!(sent.request_id, None);
}
