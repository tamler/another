//! Message importance: every level survives the database round trip, `high` and
//! `urgent` (and only those) count as urgent, a reply keeps the original's importance
//! unless it names one, and an unknown importance name is refused.

use another_core::*;

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

const LEVELS: [Importance; 4] = [
    Importance::Low,
    Importance::Normal,
    Importance::High,
    Importance::Urgent,
];

fn send(s: &Store, importance: Importance) -> Message {
    s.send_message(SendMessage {
        from: "alice".into(),
        to: vec!["bob".into()],
        subject: format!("{} news", importance.as_str()),
        body: String::new(),
        importance,
        ..Default::default()
    })
    .unwrap()
}

#[test]
fn every_importance_survives_the_round_trip_and_only_high_and_urgent_are_urgent() {
    let s = store();
    let sent: Vec<Message> = LEVELS.iter().map(|&i| send(&s, i)).collect();
    for (m, &want) in sent.iter().zip(LEVELS.iter()) {
        assert_eq!(m.importance, want);
        assert_eq!(s.message(&m.message_id).unwrap().importance, want);
    }

    let inbox = s.inbox("bob", InboxQuery::default()).unwrap();
    assert_eq!(inbox.unread, 4);
    assert_eq!(inbox.urgent_unread, 2);
    for item in &inbox.messages {
        assert_eq!(
            item.message.subject,
            format!("{} news", item.message.importance.as_str())
        );
    }

    let urgent = s
        .inbox(
            "bob",
            InboxQuery {
                urgent_only: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut levels: Vec<Importance> = urgent
        .messages
        .iter()
        .map(|i| i.message.importance)
        .collect();
    levels.sort_by_key(|i| i.as_str());
    assert_eq!(levels, [Importance::High, Importance::Urgent]);
}

#[test]
fn a_reply_keeps_the_original_importance_unless_it_names_one() {
    let s = store();
    let low = send(&s, Importance::Low);
    let kept = s
        .reply(Reply {
            from: "bob".into(),
            message_id: low.message_id.clone(),
            body: "ok".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(kept.importance, Importance::Low);
    assert_eq!(
        s.message(&kept.message_id).unwrap().importance,
        Importance::Low
    );

    let raised = s
        .reply(Reply {
            from: "bob".into(),
            message_id: low.message_id,
            body: "actually".into(),
            importance: Some(Importance::Urgent),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(raised.importance, Importance::Urgent);
}

#[test]
fn importance_names_round_trip_and_unknown_names_are_refused() {
    for level in LEVELS {
        assert_eq!(level.as_str().parse::<Importance>().unwrap(), level);
    }
    for bad in ["", "Urgent", "HIGH", "critical", "medium", " low"] {
        let err = bad.parse::<Importance>().unwrap_err();
        assert_eq!(err.code(), "invalid", "{bad:?}");
        assert!(
            err.to_string()
                .contains("expected low, normal, high or urgent"),
            "{err}"
        );
    }
}

#[test]
fn looking_up_an_unknown_message_is_not_found() {
    let s = store();
    let err = s.message("01NOSUCHMESSAGE0000000000").unwrap_err();
    assert_eq!(err.code(), "not_found", "{err}");
}

/// adv242: the urgent counts follow the unread state and both readers agree. Reading the
/// urgent message drops `urgent_unread`, and the hook's `unannounced` digest counts high and
/// urgent alike, as the inbox does.
#[test]
fn adv242_urgent_counts_follow_reads_and_the_hook_digest_counts_high_too() {
    let s = store();
    let sent: Vec<Message> = LEVELS.iter().map(|&i| send(&s, i)).collect();
    let digest = s.unannounced("bob", "", 10).unwrap();
    assert_eq!((digest.count, digest.urgent), (4, 2), "{digest:?}");

    let urgent_id = sent[3].message_id.clone();
    s.acknowledge("bob", &[urgent_id], false).unwrap();
    let inbox = s.inbox("bob", InboxQuery::default()).unwrap();
    assert_eq!(inbox.unread, 3);
    assert_eq!(inbox.urgent_unread, 1, "a read urgent message still counts");
    let digest = s.unannounced("bob", "", 10).unwrap();
    assert_eq!((digest.count, digest.urgent), (3, 1), "{digest:?}");
}

/// Every other place that reports urgent mail counts `high` too, not only the inbox: the
/// hook's unannounced notice, the heartbeat report and the work board.
#[test]
fn high_counts_as_urgent_in_the_notice_the_heartbeat_and_the_board() {
    let s = store();
    for level in LEVELS {
        send(&s, level);
    }
    let notice = s.unannounced("bob", "", 10).unwrap();
    assert_eq!((notice.count, notice.urgent), (4, 2), "unannounced");
    let beat = s.heartbeat("bob", false).unwrap();
    assert_eq!((beat.unread, beat.urgent_unread), (4, 2), "heartbeat");
    let board = s.board(BoardQuery::default()).unwrap();
    let bob = board
        .agents
        .iter()
        .find(|a| a.address.starts_with("bob"))
        .expect("bob is on the board");
    assert_eq!((bob.unread, bob.urgent_unread), (4, 2), "board");
}
