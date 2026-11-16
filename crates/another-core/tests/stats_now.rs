//! `another stats` gauges (queue v2 item 38): live holds and requests by status at the
//! moment of reading, and events per UTC day over the period.

use std::sync::atomic::Ordering;

use another_core::*;

const DAY_MS: i64 = 86_400_000;
/// 2027-01-15T12:00:00Z, the middle of a UTC day.
const T0: i64 = 1_800_000_000_000 - 1_800_000_000_000 % DAY_MS + DAY_MS / 2;

fn agent(store: &Store, name: &str) {
    store
        .register_agent(RegisterAgent {
            name: name.into(),
            program: Program::Other,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
}

fn hold(store: &Store, who: &str, project: &str, path: &str, ttl: i64) -> HoldReport {
    store
        .acquire_holds(AcquireHolds {
            agent: who.into(),
            project: project.into(),
            paths: vec![path.into()],
            ttl_seconds: Some(ttl),
            exclusive: true,
            reason: None,
            repo_wide: false,
        })
        .unwrap()
}

fn request(store: &Store, subject: &str, due: Option<i64>) -> String {
    store
        .send_message(SendMessage {
            from: "lead".into(),
            to: vec!["worker".into()],
            subject: subject.into(),
            body: "do it".into(),
            request: Some(NewRequest {
                due_in_seconds: due,
            }),
            ..Default::default()
        })
        .unwrap()
        .request_id
        .unwrap()
}

#[test]
fn stats_report_live_holds_requests_by_status_and_events_per_day() {
    let dir = tempfile::tempdir().unwrap();
    let (clock, now) = Clock::manual(T0);
    let store = Store::open_in_memory(clock).unwrap();
    let project = store
        .ensure_project("t", &dir.path().join("shop").to_string_lossy())
        .unwrap()
        .abs_path;
    for name in ["lead", "worker"] {
        agent(&store, name);
    }

    // Holds: one released, one that lapses before the read; the live one comes later.
    let gone = hold(&store, "worker", &project, "src/gone.rs", 3600);
    store
        .release_holds("worker", &[gone.granted[0].hold_id.clone()])
        .unwrap();
    hold(&store, "worker", &project, "src/lapses.rs", 60);

    // Requests: open, claimed, open but already due, and one a sweep marked stale.
    request(&store, "open", None);
    let claimed = request(&store, "claimed", None);
    store
        .update_request(RequestUpdate {
            agent: "worker".into(),
            request_id: claimed,
            action: RequestAction::Claim,
            note: None,
            due_in_seconds: None,
        })
        .unwrap();
    request(&store, "swept stale", Some(60));

    // A day later: the swept request goes stale; another lapses unswept.
    now.fetch_add(DAY_MS, Ordering::SeqCst);
    store.sweep().unwrap();
    request(&store, "due, not swept", Some(60));
    hold(&store, "lead", &project, "src/live.rs", 3600);
    now.fetch_add(120_000, Ordering::SeqCst);

    let stats = store.trial_stats(None).unwrap();
    assert_eq!(stats.now.holds_live, 1, "{:?}", stats.now);
    assert_eq!(stats.now.requests_open, 1, "{:?}", stats.now);
    assert_eq!(stats.now.requests_claimed, 1, "{:?}", stats.now);
    assert_eq!(stats.now.requests_stale, 2, "{:?}", stats.now);

    let days: Vec<&str> = stats
        .events_per_day
        .iter()
        .map(|d| d.day.as_str())
        .collect();
    assert_eq!(
        days,
        ["2027-01-15", "2027-01-16"],
        "{:?}",
        stats.events_per_day
    );
    let counted: u32 = stats.events_per_day.iter().map(|d| d.events).sum();
    let local = store
        .events(100_000)
        .unwrap()
        .into_iter()
        .filter(|e| e.node_id == store.node_id())
        .count() as u32;
    assert_eq!(counted, local, "every local event is counted once");

    // A window that starts after the first day leaves it out.
    let later = store
        .trial_stats(Some(Since::Time(T0 + DAY_MS / 2)))
        .unwrap();
    let days: Vec<&str> = later
        .events_per_day
        .iter()
        .map(|d| d.day.as_str())
        .collect();
    assert_eq!(days, ["2027-01-16"]);
    // Gauges are "now", whatever the window.
    assert_eq!(later.now.holds_live, 1);
}
