//! The dashboard shows at most the newest 40 messages of a thread and says when it cut some.
//! A cut thread keeps its root's subject, although the root message is no longer shown.

use std::sync::atomic::Ordering;

use another_core::*;

const T0: i64 = 1_800_000_000_000;

/// A thread of `total` messages: a root from alice, then replies alternating bob and alice,
/// one second apart. Returns the store and the message ids in order.
fn thread(total: usize, subject: &str) -> (Store, Vec<String>) {
    let (clock, handle) = Clock::manual(T0);
    let s = Store::open_in_memory(clock).unwrap();
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
    let root = s
        .send_message(SendMessage {
            from: "alice".into(),
            to: vec!["bob".into()],
            subject: subject.into(),
            body: "message 0".into(),
            ..Default::default()
        })
        .unwrap();
    let mut ids = vec![root.message_id.clone()];
    for n in 1..total {
        handle.fetch_add(1000, Ordering::SeqCst);
        let from = if n % 2 == 1 { "bob" } else { "alice" };
        let reply = s
            .reply(Reply {
                from: from.into(),
                message_id: ids[n - 1].clone(),
                body: format!("message {n}"),
                ..Default::default()
            })
            .unwrap();
        ids.push(reply.message_id);
    }
    (s, ids)
}

fn shown(s: &Store, root: &str) -> DashThread {
    s.dashboard_snapshot(DashboardView::LOCAL)
        .unwrap()
        .threads
        .into_iter()
        .find(|t| t.thread_id == root)
        .expect("the thread is on the dashboard")
}

#[test]
fn a_long_thread_shows_its_newest_forty_and_keeps_the_root_subject() {
    let (s, ids) = thread(45, "Migration plan");
    let t = shown(&s, &ids[0]);
    assert!(t.truncated);
    let rendered: Vec<&str> = t.messages.iter().map(|m| m.message_id.as_str()).collect();
    let newest: Vec<&str> = ids[5..].iter().map(String::as_str).collect();
    assert_eq!(rendered, newest, "the newest 40, oldest first");
    assert_eq!(
        t.subject, "Migration plan",
        "the cut root still names the thread"
    );
}

#[test]
fn a_thread_of_exactly_forty_is_shown_whole() {
    let (s, ids) = thread(40, "Short plan");
    let t = shown(&s, &ids[0]);
    assert!(!t.truncated);
    assert_eq!(t.messages.len(), 40);
    assert_eq!(t.messages[0].message_id, ids[0]);
}
