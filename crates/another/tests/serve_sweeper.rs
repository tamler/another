//! `serve::spawn_sweeper`, the background task every `another serve` (HTTP and stdio) runs:
//! it expires lapsed holds and marks overdue requests stale on its interval, and it stops
//! when the server's cancellation token fires. No test ran it before.

use std::sync::atomic::Ordering;
use std::time::Duration;

use another::serve::spawn_sweeper;
use another_core::*;
use tokio_util::sync::CancellationToken;

fn count(store: &Store, kind: &str) -> usize {
    store
        .events(10_000)
        .unwrap()
        .iter()
        .filter(|e| e.kind == kind)
        .count()
}

fn hold(store: &Store, project: &str, path: &str) {
    store
        .acquire_holds(AcquireHolds {
            agent: "alice".into(),
            project: project.into(),
            paths: vec![path.into()],
            ttl_seconds: Some(60),
            exclusive: true,
            reason: None,
            repo_wide: false,
        })
        .unwrap();
}

/// Poll `ok` for up to five seconds of real time.
async fn eventually(mut ok: impl FnMut() -> bool) -> bool {
    for _ in 0..250 {
        if ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sweeper_expires_lapsed_work_and_stops_when_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let (clock, now) = Clock::manual(1_800_000_000_000);
    let store = Store::open_in_memory(clock).unwrap().unguarded_repair();
    let project = store
        .ensure_project("shop", &dir.path().join("shop").to_string_lossy())
        .unwrap()
        .abs_path;
    for name in ["alice", "bob"] {
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
    hold(&store, &project, "src/a.rs");
    store
        .send_message(SendMessage {
            from: "alice".into(),
            to: vec!["bob".into()],
            subject: "due soon".into(),
            body: "b".into(),
            request: Some(NewRequest {
                due_in_seconds: Some(60),
            }),
            ..Default::default()
        })
        .unwrap();
    // Two minutes later both have lapsed; nothing has swept them yet.
    now.fetch_add(120_000, Ordering::SeqCst);
    assert_eq!(count(&store, "hold.expired"), 0);
    assert_eq!(count(&store, "request.stale"), 0);

    let ct = CancellationToken::new();
    spawn_sweeper(store.clone(), Duration::from_millis(20), ct.clone());
    assert!(
        eventually(|| count(&store, "hold.expired") == 1 && count(&store, "request.stale") == 1)
            .await,
        "the sweeper did not expire the hold and stale the request: {} expired, {} stale",
        count(&store, "hold.expired"),
        count(&store, "request.stale")
    );

    // It keeps sweeping: the first tick of an interval is immediate, so a sweeper that ran
    // once and stopped would pass the check above (FINDINGS-200). A hold that lapses later
    // is swept by a later tick.
    hold(&store, &project, "src/b.rs");
    now.fetch_add(120_000, Ordering::SeqCst);
    assert!(
        eventually(|| count(&store, "hold.expired") == 2).await,
        "the sweeper did not sweep again: {} expired",
        count(&store, "hold.expired")
    );

    // Once cancelled, a hold that lapses afterwards is left for the next server.
    ct.cancel();
    tokio::time::sleep(Duration::from_millis(100)).await;
    hold(&store, &project, "src/c.rs");
    now.fetch_add(120_000, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        count(&store, "hold.expired"),
        2,
        "a cancelled sweeper swept again"
    );
}
