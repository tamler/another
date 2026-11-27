//! `another stats` across the change from `push.woke` to `transport.wake`: the fields keep
//! their names and their meaning, and the activity feed reads both kinds.

use another_core::*;

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let (clock, _) = Clock::manual(1_800_000_000_000);
    let store = Store::open_with_clock(&dir.path().join("home"), clock)
        .unwrap()
        .unguarded_repair();
    for name in ["alice", "bob"] {
        store
            .register_agent(RegisterAgent {
                name: name.into(),
                program: Program::Claude,
                project: None,
                model: None,
                description: None,
            })
            .unwrap();
    }
    (dir, store)
}

fn mail(store: &Store, subject: &str) -> String {
    store
        .send_message(SendMessage {
            from: "bob".into(),
            to: vec!["alice".into()],
            subject: subject.into(),
            body: "body".into(),
            ..Default::default()
        })
        .unwrap()
        .message_id
}

/// An event as a build before the transport interface wrote it. The current build has no
/// code that writes `push.woke`, so the row goes straight into the log.
fn old_wake(dir: &tempfile::TempDir, store: &Store, n: u32, outcome: &str, message_ids: &[String]) {
    let agent = store.agent("alice").unwrap();
    let payload = serde_json::json!({
        "agent_id": agent.agent_id,
        "agent": "alice",
        "via": "herdr",
        "target": "w1:p1",
        "outcome": outcome,
        "detail": "agent was idle",
        "message_ids": message_ids,
    });
    rusqlite::Connection::open(dir.path().join("home").join(DB_FILE))
        .unwrap()
        .execute(
            "INSERT INTO events (event_id, node_id, ts, actor, kind, payload)
             VALUES (?1, ?2, ?3, 'alice', 'push.woke', ?4)",
            rusqlite::params![
                format!("old-wake-{n}"),
                store.node_id(),
                store.now(),
                payload.to_string()
            ],
        )
        .unwrap();
}

fn wake(store: &Store, outcome: WakeOutcome, trigger: &str, message_ids: Vec<String>) {
    store
        .record_transport_wake(&TransportWake {
            agent: "alice".into(),
            session_id: Some("s1".into()),
            transport: "herdr".into(),
            target: "w1:p1".into(),
            outcome,
            detail: "what herdr said".into(),
            kind: WakeKind::Mail,
            message_ids,
            trigger: trigger.into(),
        })
        .unwrap();
}

#[test]
fn stats_counts_daemon_wakes_as_before() {
    let (dir, store) = store();
    let old = mail(&store, "woken by an old build");
    let new = mail(&store, "woken by this build");
    let manual = mail(&store, "woken by hand");

    // What an old build wrote: one sent wake and one failed wake.
    old_wake(&dir, &store, 1, "sent", std::slice::from_ref(&old));
    old_wake(&dir, &store, 2, "failed", &[]);
    let s = store.trial_stats(None).unwrap();
    assert_eq!((s.wakes_sent, s.wakes_failed, s.messages_pushed), (1, 1, 1));

    // The daemon's wakes of this build: `started` counts where `sent` counted, a failure
    // counts as a failed wake, and the other outcomes count as neither.
    wake(&store, WakeOutcome::Started, "daemon", vec![new.clone()]);
    for failure in [
        WakeOutcome::NotStarted,
        WakeOutcome::Undelivered,
        WakeOutcome::Unknown,
    ] {
        wake(&store, failure, "daemon", Vec::new());
    }
    for neither in [
        WakeOutcome::Unconfirmed,
        WakeOutcome::RefusedBlocked,
        WakeOutcome::Gone,
    ] {
        wake(&store, neither, "daemon", Vec::new());
    }
    let s = store.trial_stats(None).unwrap();
    assert_eq!((s.wakes_sent, s.wakes_failed), (2, 4));
    assert_eq!(s.messages_pushed, 2, "one by the old wake, one by the new");

    // Only the daemon wrote `push.woke`, so wakes by hand and by `--all` are not counted.
    wake(&store, WakeOutcome::Started, "manual", vec![manual.clone()]);
    wake(&store, WakeOutcome::NotStarted, "manual", Vec::new());
    wake(&store, WakeOutcome::Started, "all", vec![manual.clone()]);
    wake(&store, WakeOutcome::Unknown, "all", Vec::new());
    let s = store.trial_stats(None).unwrap();
    assert_eq!((s.wakes_sent, s.wakes_failed, s.messages_pushed), (2, 4, 2));
    assert_eq!(s.pushes_acted_on, 0);

    // Acting on woken mail still counts, for old and new events alike.
    store.acknowledge("alice", &[old, new], false).unwrap();
    let s = store.trial_stats(None).unwrap();
    assert_eq!(s.pushes_acted_on, 2);

    // The recent list: the daemon's wakes, newest first, old and new.
    assert_eq!(s.wakes.len(), 9, "{:?}", s.wakes);
    assert_eq!(s.wakes[0].outcome, "gone");
    assert_eq!(s.wakes[6].outcome, "started");
    assert_eq!(
        (s.wakes[6].via.as_str(), s.wakes[6].target.as_str()),
        ("herdr", "w1:p1")
    );
    assert_eq!(s.wakes[6].messages, 1);
    assert_eq!(s.wakes[8].outcome, "sent");
    assert_eq!(s.wakes[8].via, "herdr");

    // The field names of the JSON form are the ones `another stats --json` always had.
    let json = serde_json::to_value(&s).unwrap();
    for field in [
        "since",
        "until",
        "edits_blocked",
        "collisions",
        "holds_auto_acquired",
        "notices_mid_task",
        "notices_prompt",
        "wakes_sent",
        "wakes_failed",
        "messages_pushed",
        "pushes_acted_on",
        "requests_created",
        "requests_claimed",
        "requests_completed",
        "blocked",
        "wakes",
    ] {
        assert!(json.get(field).is_some(), "{field}");
    }
    assert_eq!(json.as_object().unwrap().len(), 16);
    for field in ["at", "agent", "via", "target", "outcome", "messages"] {
        assert!(json["wakes"][0].get(field).is_some(), "{field}");
    }
    assert_eq!(json["wakes"][0].as_object().unwrap().len(), 6);
}

/// The activity feed says what happened for every outcome, and still reads old events.
#[test]
fn activity_reads_old_and_new_wake_events() {
    let (dir, store) = store();
    old_wake(&dir, &store, 1, "sent", &[]);
    old_wake(&dir, &store, 2, "failed", &[]);
    for outcome in [
        WakeOutcome::Started,
        WakeOutcome::NotStarted,
        WakeOutcome::Unconfirmed,
        WakeOutcome::RefusedBlocked,
        WakeOutcome::Gone,
        WakeOutcome::Undelivered,
        WakeOutcome::Unknown,
    ] {
        wake(&store, outcome, "daemon", Vec::new());
    }
    let feed = store
        .activity(ActivityQuery {
            since: None,
            limit: 200,
            kinds: Vec::new(),
        })
        .unwrap();
    let text: Vec<&str> = feed
        .items
        .iter()
        .filter(|i| i.kind == "push.woke" || i.kind == TRANSPORT_WAKE_EVENT)
        .map(|i| i.summary.as_str())
        .collect();
    assert_eq!(
        text,
        [
            "woke alice in herdr w1:p1",
            "could not wake alice in herdr w1:p1",
            "woke alice",
            "woke alice but it did not start working",
            "sent a wake to alice; not confirmed",
            "did not wake alice: waiting on a person",
            "did not wake alice: no such session",
            "could not wake alice",
            "could not wake alice",
        ]
    );
}

/// A wake's record and the release of its announcement claim are one transaction.
#[test]
fn a_recorded_wake_gives_the_claim_back_only_when_its_outcome_says_so() {
    let (_dir, store) = store();
    let waiting = |store: &Store| store.unannounced("alice", "", 100).unwrap().items.len();
    for (outcome, released) in [
        (WakeOutcome::Started, false),
        (WakeOutcome::Unconfirmed, false),
        (WakeOutcome::NotStarted, true),
        (WakeOutcome::Unknown, true),
        (WakeOutcome::RefusedBlocked, true),
        (WakeOutcome::Gone, true),
        (WakeOutcome::Undelivered, true),
    ] {
        let id = mail(&store, &format!("for {}", outcome.as_str()));
        let before = waiting(&store);
        let claimed = store
            .claim_announcement("alice", std::slice::from_ref(&id), "wake", None)
            .unwrap();
        assert_eq!(claimed, std::slice::from_ref(&id));
        assert_eq!(waiting(&store), before - 1);
        wake(&store, outcome, "daemon", claimed);
        let after = if released { before } else { before - 1 };
        assert_eq!(waiting(&store), after, "{outcome:?}");
    }
}
