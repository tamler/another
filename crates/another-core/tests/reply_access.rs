//! A remote token may reply only in threads its agent is already in
//! (`Store::check_reply_access`, used by MCP `send_message` with `reply_to`). Any other
//! message looks exactly like one that does not exist.

use another_core::*;

fn store() -> (Store, Vec<Agent>) {
    let (clock, _) = Clock::manual(1_800_000_000_000);
    let store = Store::open_in_memory(clock).unwrap();
    let agents = ["alice", "bob", "carol", "dave"]
        .iter()
        .map(|name| {
            store
                .register_agent(RegisterAgent {
                    name: (*name).into(),
                    program: Program::Other,
                    project: None,
                    model: None,
                    description: None,
                })
                .unwrap()
        })
        .collect();
    (store, agents)
}

fn token_for(agent: &Agent) -> TokenScope {
    TokenScope {
        token_id: "tok".into(),
        agent_id: agent.agent_id.clone(),
        agent: agent.name.clone(),
        project_globs: vec![],
    }
}

fn send(store: &Store, from: &str, to: &[&str], cc: &[&str], reply_to: Option<&str>) -> Message {
    store
        .send_message(SendMessage {
            from: from.into(),
            to: to.iter().map(|s| s.to_string()).collect(),
            cc: cc.iter().map(|s| s.to_string()).collect(),
            subject: "Plan".into(),
            body: "text".into(),
            reply_to: reply_to.map(str::to_string),
            ..Default::default()
        })
        .unwrap()
}

#[test]
fn sender_recipient_and_cc_may_reply_an_outsider_may_not() {
    let (store, a) = store();
    let (alice, bob, carol, dave) = (&a[0], &a[1], &a[2], &a[3]);
    let root = send(&store, "alice", &["bob"], &["carol"], None);

    for member in [alice, bob, carol] {
        store
            .check_reply_access(&token_for(member), &root.message_id)
            .unwrap_or_else(|e| panic!("{} is in the thread: {e}", member.name));
    }
    let err = store
        .check_reply_access(&token_for(dave), &root.message_id)
        .unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err:?}");
}

#[test]
fn a_refused_message_looks_like_a_missing_one() {
    let (store, a) = store();
    let dave = &a[3];
    let real = send(&store, "alice", &["bob"], &[], None).message_id;
    // A well-formed id that names no message.
    let missing = format!("{}X", &real[..real.len() - 1]);
    assert_ne!(missing, real);

    let refused = store
        .check_reply_access(&token_for(dave), &real)
        .unwrap_err()
        .to_string();
    let unknown = store
        .check_reply_access(&token_for(dave), &missing)
        .unwrap_err()
        .to_string();
    // Same wording apart from the id the caller passed, so the error says nothing about
    // whether the message exists.
    assert_eq!(
        refused.replace(&real, "ID"),
        unknown.replace(&missing, "ID")
    );
}

#[test]
fn joining_a_thread_later_grants_access_to_its_earlier_messages() {
    let (store, a) = store();
    let dave = &a[3];
    let root = send(&store, "alice", &["bob"], &[], None);
    assert!(store
        .check_reply_access(&token_for(dave), &root.message_id)
        .is_err());

    // Bob pulls dave into the same thread.
    let reply = send(&store, "bob", &["dave"], &[], Some(&root.message_id));
    assert_eq!(reply.thread_id, root.thread_id);
    store
        .check_reply_access(&token_for(dave), &root.message_id)
        .unwrap();
    store
        .check_reply_access(&token_for(dave), &reply.message_id)
        .unwrap();

    // Membership is per thread: a separate thread between the same people stays closed.
    let other = send(&store, "alice", &["bob"], &[], None);
    assert_ne!(other.thread_id, root.thread_id);
    assert!(store
        .check_reply_access(&token_for(dave), &other.message_id)
        .is_err());
}
