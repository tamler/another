//! Local dashboard read model: presence, conflicts, bodies, and the two human actions.

use std::sync::atomic::Ordering;

use another_core::*;

const T0: i64 = 1_800_000_000_000;

fn setup() -> (Store, std::sync::Arc<std::sync::atomic::AtomicI64>) {
    let dir = tempfile::tempdir().unwrap();
    let shop = dir.path().join("work/shop").to_string_lossy().into_owned();
    let api = dir.path().join("work/api").to_string_lossy().into_owned();
    let (clock, handle) = Clock::manual(T0);
    let store = Store::open_in_memory(clock).unwrap();
    store
        .register_agent(RegisterAgent {
            name: "dave".into(),
            program: Program::Codex,
            project: Some(shop.clone()),
            model: None,
            description: Some("old session".into()),
        })
        .unwrap();
    handle.fetch_add(48 * 3_600_000, Ordering::SeqCst);
    for (name, program, project) in [
        ("alice", Program::Claude, shop.as_str()),
        ("bob", Program::Codex, shop.as_str()),
        ("carol", Program::Grok, api.as_str()),
    ] {
        store
            .register_agent(RegisterAgent {
                name: name.into(),
                program,
                project: Some(project.into()),
                model: None,
                description: None,
            })
            .unwrap();
    }
    let msg = store
        .send_message(SendMessage {
            from: "alice".into(),
            to: vec!["bob".into()],
            subject: "Migration plan".into(),
            body: "SECRET BODY please review the cutover notes".into(),
            importance: Importance::High,
            request: Some(NewRequest {
                due_in_seconds: Some(3_600),
            }),
            ..Default::default()
        })
        .unwrap();
    store
        .reply(Reply {
            from: "bob".into(),
            message_id: msg.message_id.clone(),
            body: "Looking now.".into(),
            ..Default::default()
        })
        .unwrap();
    store
        .acquire_holds(AcquireHolds {
            agent: "alice".into(),
            project: "shop".into(),
            paths: vec!["src/**".into()],
            ttl_seconds: Some(4 * 3_600),
            exclusive: true,
            reason: Some("editing the router".into()),
            repo_wide: false,
        })
        .unwrap();
    let bob_hold = store
        .acquire_holds(AcquireHolds {
            agent: "bob".into(),
            project: "shop".into(),
            paths: vec!["docs/**".into()],
            ttl_seconds: Some(4 * 3_600),
            exclusive: true,
            reason: Some("tests".into()),
            repo_wide: false,
        })
        .unwrap();
    assert!(
        bob_hold.granted.len() == 1 && bob_hold.conflicts.is_empty(),
        "docs/** does not overlap src/**, so both holds exist"
    );
    store
        .register_resource(RegisterResource {
            agent: "alice".into(),
            kind: ResourceKind::ModelServer,
            name: "ollama@11434".into(),
            endpoint: Some("http://127.0.0.1:11434".into()),
            port: None,
            pid: Some(4242),
            project: Some("shop".into()),
            purpose: Some("local llm".into()),
            mode: LeaseMode::Shared,
            ttl_seconds: Some(3_600),
            metadata: None,
        })
        .unwrap();
    store
        .borrow_resource(
            "bob",
            &ResourceRef::Name(ResourceKind::ModelServer, "ollama@11434".into()),
            Some(3_600),
        )
        .unwrap();
    handle.fetch_add(40 * 60 * 1000, Ordering::SeqCst);
    store.heartbeat("alice", false).unwrap();
    store.heartbeat("carol", true).unwrap();
    (store, handle)
}

fn agent<'a>(snap: &'a DashboardSnapshot, name: &str) -> &'a DashAgent {
    snap.agents
        .iter()
        .find(|a| a.name == name)
        .unwrap_or_else(|| panic!("missing agent {name}"))
}

#[test]
fn snapshot_hides_nothing_but_marks_stale_and_shows_local_bodies() {
    let (store, _) = setup();
    let snap = store.dashboard_snapshot(DashboardView::LOCAL).unwrap();

    assert_eq!(agent(&snap, "alice").presence, "live");
    assert!(!agent(&snap, "alice").stale);
    assert_eq!(agent(&snap, "bob").presence, "idle");
    assert_eq!(agent(&snap, "carol").presence, "offline");
    assert!(!agent(&snap, "carol").stale, "just ended, still shown");
    assert_eq!(agent(&snap, "dave").presence, "offline");
    assert!(agent(&snap, "dave").stale, "two days quiet");
    assert_eq!(snap.counts.live, 1);
    assert_eq!(snap.counts.idle, 1);
    assert_eq!(snap.counts.stale, 1);
    assert!(snap.counts.unread >= 1);

    assert_eq!(
        snap.counts.conflicts, 0,
        "acquire refuses overlapping holds"
    );
    let bob_hold = snap
        .holds
        .iter()
        .find(|h| h.hold.agent.starts_with("bob@"))
        .unwrap();
    assert!(bob_hold.releasable, "bob has been idle long enough");
    assert!(bob_hold.conflicts.is_empty());
    let alice_hold = snap
        .holds
        .iter()
        .find(|h| h.hold.agent.starts_with("alice@"))
        .unwrap();
    assert!(!alice_hold.releasable, "alice is live and unconflicted");

    assert_eq!(snap.counts.requests_open, 1);
    assert_eq!(snap.requests[0].to, vec!["bob@shop".to_string()]);
    assert!(snap.requests[0].expirable);

    let thread = snap
        .threads
        .iter()
        .find(|t| t.subject == "Migration plan")
        .unwrap();
    assert!(thread.participants.iter().any(|p| p == "alice@shop"));
    assert!(thread.participants.iter().any(|p| p == "bob@shop"));
    let secret = thread
        .messages
        .iter()
        .find(|m| m.body.as_deref().unwrap_or("").contains("SECRET BODY"))
        .expect("local thread shows the message body");
    assert!(!secret.body_redacted);

    assert_eq!(snap.counts.resources_live, 1);
    assert_eq!(snap.resources[0].leases.len(), 2);
    assert_eq!(snap.access.mode, "local");
    assert!(snap.access.actions);

    let redacted = store.dashboard_snapshot(DashboardView::REDACTED).unwrap();
    assert!(redacted.threads[0]
        .messages
        .iter()
        .all(|m| m.body.is_none()));
    assert!(redacted.threads[0].messages.iter().all(|m| m.body_redacted));
    assert!(!redacted.access.actions);

    let feed = store.activity(ActivityQuery::default()).unwrap();
    let text = feed
        .items
        .iter()
        .map(|i| i.summary.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!text.contains("SECRET BODY"), "{text}");
}

#[test]
fn timeline_filters_and_human_actions_are_limited() {
    let (store, _) = setup();
    let page = store
        .dashboard_activity(DashActivityQuery {
            text: Some("SECRET BODY".into()),
            include_bodies: true,
            ..Default::default()
        })
        .unwrap();
    assert!(
        page.items
            .iter()
            .any(|i| i.body.as_deref().unwrap_or("").contains("SECRET BODY")),
        "local search can see a local body"
    );
    let hidden = store
        .dashboard_activity(DashActivityQuery {
            text: Some("SECRET BODY".into()),
            include_bodies: false,
            ..Default::default()
        })
        .unwrap();
    assert!(
        hidden.items.is_empty(),
        "a redacted view does not search or return bodies"
    );

    let shop = store
        .dashboard_activity(DashActivityQuery {
            project: Some("shop".into()),
            kinds: vec![ActivityKind::Hold],
            ..Default::default()
        })
        .unwrap();
    assert!(shop.items.iter().all(|i| i.category == ActivityKind::Hold));
    assert!(shop
        .items
        .iter()
        .all(|i| i.project.as_deref() == Some("shop")));
    assert!(!shop.items.is_empty());

    let bob_hold = store
        .holds(HoldFilter::default())
        .unwrap()
        .into_iter()
        .find(|h| h.agent.starts_with("bob@"))
        .unwrap();
    store
        .release_hold_from_dashboard(&bob_hold.hold_id)
        .unwrap();
    let alice_hold = store
        .holds(HoldFilter::default())
        .unwrap()
        .into_iter()
        .find(|h| h.agent.starts_with("alice@"))
        .unwrap();
    let err = store
        .release_hold_from_dashboard(&alice_hold.hold_id)
        .unwrap_err();
    assert_eq!(err.code(), "conflict", "{err}");

    let request_id = store
        .requests(RequestFilter {
            limit: 10,
            ..Default::default()
        })
        .unwrap()[0]
        .request_id
        .clone();
    let expired = store.expire_request_from_dashboard(&request_id).unwrap();
    assert_eq!(expired.status, RequestStatus::Stale);
    let again = store.expire_request_from_dashboard(&request_id).unwrap();
    assert_eq!(again.status, RequestStatus::Stale);

    let feed = store.activity(ActivityQuery::default()).unwrap();
    let text = feed
        .items
        .iter()
        .map(|i| i.summary.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("from the local dashboard"), "{text}");
    assert!(
        text.contains("request went stale from the local dashboard"),
        "{text}"
    );
}
