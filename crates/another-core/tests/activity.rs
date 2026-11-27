//! Activity feed and session lifecycle events.

use std::sync::atomic::Ordering;

use another_core::*;

const T0: i64 = 1_800_000_000_000;

fn setup() -> (Store, std::sync::Arc<std::sync::atomic::AtomicI64>) {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("work/app").to_string_lossy().into_owned();
    let (clock, handle) = Clock::manual(T0);
    let store = Store::open_in_memory(clock).unwrap().unguarded_repair();
    for (name, program) in [("alice", Program::Claude), ("bob", Program::Grok)] {
        store
            .register_agent(RegisterAgent {
                name: name.into(),
                program,
                project: Some(project.clone()),
                model: None,
                description: None,
            })
            .unwrap();
    }
    (store, handle)
}

#[test]
fn feed_summarizes_without_bodies() {
    let (store, clock) = setup();
    let start = store
        .session(
            "alice",
            SessionPhase::Start,
            SessionInfo {
                client: Some("claude".into()),
                session_id: Some("s-1".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(start.status, "active");
    clock.fetch_add(1000, Ordering::SeqCst);
    let msg = store
        .send_message(SendMessage {
            from: "alice".into(),
            to: vec!["bob".into()],
            subject: "Ship it".into(),
            body: "SECRET BODY TEXT".into(),
            importance: Importance::Urgent,
            request: Some(NewRequest::default()),
            ..Default::default()
        })
        .unwrap();
    store
        .acknowledge("bob", std::slice::from_ref(&msg.message_id), true)
        .unwrap();
    store
        .acquire_holds(AcquireHolds {
            agent: "bob".into(),
            project: "app".into(),
            paths: vec!["src/**".into()],
            ttl_seconds: None,
            exclusive: true,
            reason: None,
            repo_wide: false,
        })
        .unwrap();
    store.heartbeat("bob", true).unwrap();

    let feed = store.activity(ActivityQuery::default()).unwrap();
    let summaries: Vec<&str> = feed.items.iter().map(|i| i.summary.as_str()).collect();
    let text = summaries.join("\n");
    assert!(!text.contains("SECRET BODY TEXT"), "{text}");
    assert!(
        text.contains("alice@app started a session (claude)"),
        "{text}"
    );
    assert!(
        text.contains("alice@app sent request to bob@app: Ship it [urgent]"),
        "{text}"
    );
    assert!(text.contains("bob@app acknowledged \"Ship it\""), "{text}");
    assert!(
        text.contains("bob@app holds app:src/** (exclusive)"),
        "{text}"
    );
    assert!(
        text.contains("bob@app released 1 hold(s) at session end"),
        "{text}"
    );
    assert!(text.contains("bob@app ended a session"), "{text}");
    let started = feed
        .items
        .iter()
        .find(|i| i.kind == "session.started")
        .unwrap();
    assert_eq!(started.category, ActivityKind::Session);
    assert_eq!(started.refs["session_id"], "s-1");
    // Oldest first, ending at the cursor.
    assert_eq!(
        feed.next_since.as_deref(),
        Some(feed.items.last().unwrap().event_id.as_str())
    );
}

#[test]
fn feed_filters_pages_and_limits() {
    let (store, clock) = setup();
    for i in 0..5 {
        clock.fetch_add(1000, Ordering::SeqCst);
        store
            .send_message(SendMessage {
                from: "alice".into(),
                to: vec!["bob".into()],
                subject: format!("m{i}"),
                body: String::new(),
                ..Default::default()
            })
            .unwrap();
    }
    let mail_only = ActivityQuery {
        kinds: vec![ActivityKind::Mail],
        ..Default::default()
    };
    let feed = store.activity(mail_only).unwrap();
    assert_eq!(feed.items.len(), 5);
    assert!(feed.items.iter().all(|i| i.category == ActivityKind::Mail));

    // Without since, the newest N; with an event-id cursor, what came after it.
    let last2 = store
        .activity(ActivityQuery {
            limit: 2,
            kinds: vec![ActivityKind::Mail],
            ..Default::default()
        })
        .unwrap();
    assert!(last2.items[1].summary.ends_with("m4"));
    let first = store
        .activity(ActivityQuery {
            kinds: vec![ActivityKind::Mail],
            limit: 1,
            since: Some(Since::Time(T0)),
        })
        .unwrap();
    assert!(first.items[0].summary.ends_with("m0"));
    let next = store
        .activity(ActivityQuery {
            kinds: vec![ActivityKind::Mail],
            limit: 10,
            since: Some(Since::After(first.next_since.clone().unwrap())),
        })
        .unwrap();
    assert_eq!(next.items.len(), 4);
    assert!(next.items[0].summary.ends_with("m1"));

    // Time window.
    let recent = store
        .activity(ActivityQuery {
            since: Some(Since::parse("2s", store.now()).unwrap()),
            kinds: vec![ActivityKind::Mail],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(recent.items.len(), 3);

    let bad = store.activity(ActivityQuery {
        since: Some(Since::After("01ARZ3NDEKTSV4RRFFQ69G5FAV".into())),
        ..Default::default()
    });
    assert_eq!(bad.unwrap_err().code(), "not_found");
}

#[test]
fn summary_counts_and_digest() {
    let (store, clock) = setup();
    store
        .session("alice", SessionPhase::Start, SessionInfo::default())
        .unwrap();
    store
        .send_message(SendMessage {
            from: "alice".into(),
            to: vec!["bob".into()],
            subject: "Please review".into(),
            body: String::new(),
            request: Some(NewRequest::default()),
            ..Default::default()
        })
        .unwrap();
    store
        .acquire_holds(AcquireHolds {
            agent: "alice".into(),
            project: "app".into(),
            paths: vec!["docs/**".into()],
            ttl_seconds: None,
            exclusive: false,
            reason: None,
            repo_wide: false,
        })
        .unwrap();
    let sum = store.activity_summary(None).unwrap();
    assert_eq!(sum.counts["mail"], 1);
    assert_eq!(sum.counts["request"], 1);
    assert_eq!(sum.counts["hold"], 1);
    assert_eq!(sum.requests.open, 1);
    assert_eq!(sum.live_holds, 1);
    assert!(sum.agents.iter().any(|a| a.agent == "alice@app"));
    assert!(sum.text.contains("1 open"), "{}", sum.text);
    assert!(sum.highlights.iter().any(|h| h.contains("Please review")));

    // Two hours later only the hold's expiry is in the window; request state remains.
    clock.fetch_add(2 * 3_600_000, Ordering::SeqCst);
    let later = store.activity_summary(None).unwrap();
    assert_eq!(later.highlights.len(), 1, "{:?}", later.highlights);
    assert!(later.highlights[0].contains("expired"));
    assert_eq!(later.live_holds, 0);
    assert_eq!(later.requests.open, 1);
}

/// R15 (D-next): `sync` is a category of its own. It holds the events a sync skipped and
/// their acknowledgements and nothing else; the records a sync writes about its cursors
/// stay under `federation`.
#[test]
fn kinds_sync_lists_only_sync_events() {
    let open = |name: &str| {
        let dir = tempfile::tempdir().unwrap();
        let (clock, _) = Clock::manual(T0);
        let store = Store::open_with_clock(dir.path(), clock)
            .unwrap()
            .unguarded_repair();
        store.set_node_name(name).unwrap();
        (dir, store)
    };
    let (_hub_dir, hub) = open("hub");
    let (_spoke_dir, spoke) = open("spoke");
    let code = hub
        .create_invite("https://hub.example", &Perm::ALL, 3600)
        .unwrap();
    let (invite, req) = spoke.prepare_join(&code).unwrap();
    let resp = hub.accept_pairing(&req).unwrap();
    spoke.complete_join(&invite, &resp).unwrap();
    for (store, name) in [(&hub, "bob"), (&spoke, "alice")] {
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
    // One sync by hand, with a skip on the way: it leaves `peer.pushed`, `peer.anchored`
    // and the marker in the spoke's log, beside the `peer.paired` of the pairing.
    let skipped = SkippedEvent {
        event_id: "01SKIPPED".into(),
        event_node: spoke.node_id().into(),
        event_kind: "agent.registered".into(),
        event_actor: "system".into(),
        bytes: 9_437_500,
        about: Some("agent wordy".into()),
        route: "directory".into(),
        route_nodes: Vec::new(),
    };
    let (events, high) = spoke.outbound_events(0, 500).unwrap();
    hub.ingest(spoke.node_id(), &events, &[]).unwrap();
    let last = events.last().map(|e| e.event_id.as_str());
    spoke
        .advance_push_past(hub.node_id(), high, last, std::slice::from_ref(&skipped))
        .unwrap();
    let page = hub.pull_for(spoke.node_id(), 0, 500).unwrap();
    spoke
        .ingest(hub.node_id(), &page.events, &page.nodes)
        .unwrap();
    let last = page.events.last().map(|e| (e.event_id.as_str(), e.seq));
    spoke
        .advance_pull(hub.node_id(), page.cursor, last)
        .unwrap();
    spoke.acknowledge_skip("01SKIPPED").unwrap();

    let feed = |kinds: Vec<ActivityKind>| {
        spoke
            .activity(ActivityQuery {
                since: None,
                limit: 500,
                kinds,
            })
            .unwrap()
            .items
    };
    let sync = feed(vec![ActivityKind::Sync]);
    let kinds: Vec<&str> = sync.iter().map(|i| i.kind.as_str()).collect();
    assert_eq!(kinds, ["sync.skipped", "sync.skip_acked"]);
    assert!(sync.iter().all(|i| i.category == ActivityKind::Sync));
    assert_eq!(
        sync[0].summary,
        "skipped event 01SKIPPED (agent.registered, 9437500 bytes, written by system): too \
         large to send; agent wordy"
    );
    assert_eq!(
        sync[0].refs,
        serde_json::json!({ "event_id": "01SKIPPED", "event_node": spoke.node_id() })
    );
    assert_eq!(
        sync[1].summary,
        "acknowledged the skip of event 01SKIPPED; it stays in the log and is not sent"
    );
    assert_eq!(sync[1].refs, serde_json::json!({ "event_id": "01SKIPPED" }));

    let federation = feed(vec![ActivityKind::Federation]);
    let kinds: Vec<&str> = federation.iter().map(|i| i.kind.as_str()).collect();
    for kind in ["peer.paired", "peer.pushed", "peer.anchored"] {
        assert!(kinds.contains(&kind), "{kind} is missing from {kinds:?}");
    }
    assert!(kinds.iter().all(|k| k.starts_with("peer.")), "{kinds:?}");

    assert_eq!(ActivityKind::of_event("sync.skipped"), ActivityKind::Sync);
    assert_eq!(ActivityKind::of_event("sync.anything"), ActivityKind::Sync);
    assert_eq!("sync".parse::<ActivityKind>().unwrap(), ActivityKind::Sync);
    assert_eq!(
        "nosuch".parse::<ActivityKind>().unwrap_err().to_string(),
        "unknown activity kind \"nosuch\"; expected mail, request, hold, resource, session, \
         system, federation, queue or sync"
    );
}
