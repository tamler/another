//! Federation soak: a hub that restarts, a hub restored from an older backup, and a spoke
//! that is offline for days, all over real HTTP through `fed::sync`.
//!
//! The hub verifies signed requests against its own clock within five minutes, and
//! `fed::sync` signs with the wall clock, so the hub always runs on the system clock. A
//! spoke that was "offline for days" runs on a manual clock that starts days in the past
//! and catches up to now, so its backlog carries genuinely old timestamps.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use another::fed;
use another::serve::{http_router, HttpOptions};
use another_core::*;
use tokio_util::sync::CancellationToken;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

fn wall_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

struct Spoke {
    store: Store,
    home: PathBuf,
}

fn spoke(root: &Path, name: &str, clock: Clock) -> Spoke {
    let home = root.join(name);
    let store = Store::open_with_clock(&home, clock).unwrap();
    store.set_node_name(name).unwrap();
    Spoke { store, home }
}

fn register(store: &Store, name: &str, project: Option<&Path>) {
    if let Some(p) = project {
        std::fs::create_dir_all(p).unwrap();
    }
    store
        .register_agent(RegisterAgent {
            name: name.into(),
            program: Program::Claude,
            project: project.map(|p| p.to_string_lossy().into_owned()),
            model: None,
            description: None,
        })
        .unwrap();
}

/// A hub serving federation over HTTP on a fixed loopback port, restartable in place.
struct Hub {
    home: PathBuf,
    port: u16,
    store: Store,
    ct: CancellationToken,
    rt: Option<tokio::runtime::Runtime>,
}

impl Hub {
    fn start(root: &Path) -> Hub {
        let home = root.join("hub");
        let store = Store::open(&home).unwrap();
        store.set_node_name("hub").unwrap();
        let mut hub = Hub {
            home,
            port: 0,
            store,
            ct: CancellationToken::new(),
            rt: None,
        };
        hub.serve();
        hub
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn serve(&mut self) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener = rt
            .block_on(tokio::net::TcpListener::bind(("127.0.0.1", self.port)))
            .unwrap();
        self.port = listener.local_addr().unwrap().port();
        let ct = CancellationToken::new();
        let opts = HttpOptions {
            federation: true,
            bound_port: self.port,
            ..Default::default()
        };
        let app = http_router(self.store.clone(), ct.clone(), &opts);
        let stop = ct.clone();
        rt.spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move { stop.cancelled_owned().await })
            .await
            .unwrap();
        });
        self.ct = ct;
        self.rt = Some(rt);
    }

    /// Stop the server and close the database, as a process exit would.
    fn stop(&mut self) {
        self.ct.cancel();
        if let Some(rt) = self.rt.take() {
            rt.shutdown_timeout(std::time::Duration::from_secs(5));
        }
    }

    /// Stop, reopen the same home, and serve again on the same port.
    fn restart(&mut self) {
        self.stop();
        self.store = Store::open(&self.home).unwrap();
        self.serve();
    }

    /// Stop, replace the database with `backup` as an operator restoring a copy would, and
    /// serve again. Every connection to the old file is closed first: Windows cannot delete
    /// an open file, and a leftover WAL would replay the newer writes into the copy.
    fn restore(&mut self, backup: &Path) {
        self.stop();
        // The server's clones went with its runtime; dropping ours closes the file.
        self.store = Store::open_in_memory(Clock::System).unwrap();
        for suffix in ["", "-wal", "-shm"] {
            match std::fs::remove_file(self.home.join(format!("{DB_FILE}{suffix}"))) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => panic!("cannot remove {DB_FILE}{suffix}: {e}"),
            }
        }
        std::fs::copy(backup, self.home.join(DB_FILE)).unwrap();
        self.restart();
    }

    fn join(&self, spoke: &Spoke) {
        let code = self
            .store
            .create_invite(&self.url(), &Perm::ALL, 3600)
            .unwrap();
        fed::join(&spoke.store, &code).unwrap();
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.stop();
    }
}

fn sync(s: &Spoke) -> fed::SyncReport {
    let r = fed::sync(&s.store).unwrap();
    assert!(r.push_rejected.is_empty(), "{:?}", r.push_rejected);
    assert!(r.pull_rejected.is_empty(), "{:?}", r.pull_rejected);
    r
}

fn send(store: &Store, from: &str, to: &str, subject: &str) {
    store
        .send_message(SendMessage {
            from: from.into(),
            to: vec![to.into()],
            subject: subject.into(),
            body: format!("body: {subject}"),
            ..Default::default()
        })
        .unwrap();
}

/// Subjects in `who`'s inbox that start with `prefix`.
fn subjects(store: &Store, who: &str, prefix: &str) -> HashSet<String> {
    store
        .inbox(
            who,
            InboxQuery {
                unread_only: false,
                urgent_only: false,
                limit: 100_000,
            },
        )
        .unwrap()
        .messages
        .into_iter()
        .map(|i| i.message.subject)
        .filter(|s| s.starts_with(prefix))
        .collect()
}

fn event_ids(store: &Store) -> HashSet<String> {
    store
        .events(u32::MAX)
        .unwrap()
        .into_iter()
        .map(|e| e.event_id)
        .collect()
}

/// Every event the hub would relay to `s` is on `s`, and every event `s` would push is on
/// the hub. Uses the system's own selection (`pull_for`, `outbound_events`) so this checks
/// transport (paging, cursors, restarts), not classification.
fn assert_converged(hub: &Hub, s: &Spoke) {
    let on_spoke = event_ids(&s.store);
    let relay = hub
        .store
        .pull_for(s.store.node_id(), 0, u32::MAX / 8)
        .unwrap();
    let missing: Vec<_> = relay
        .events
        .iter()
        .filter(|e| !on_spoke.contains(&e.event_id))
        .map(|e| (e.seq, e.kind.clone()))
        .collect();
    assert!(
        missing.is_empty(),
        "{} of {} relayable hub events never reached the spoke: {:?}",
        missing.len(),
        relay.events.len(),
        &missing[..missing.len().min(10)]
    );
    let on_hub = event_ids(&hub.store);
    let (own, _) = s.store.outbound_events(0, u32::MAX / 8).unwrap();
    let unpushed: Vec<_> = own
        .iter()
        .filter(|e| !on_hub.contains(&e.event_id))
        .map(|e| (e.seq, e.kind.clone()))
        .collect();
    assert!(
        unpushed.is_empty(),
        "{} of {} spoke events never reached the hub: {:?}",
        unpushed.len(),
        own.len(),
        &unpushed[..unpushed.len().min(10)]
    );
}

fn count(db: &Path, table: &str) -> i64 {
    rusqlite::Connection::open(db.join(DB_FILE))
        .unwrap()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn counts(home: &Path) -> Vec<(&'static str, i64)> {
    ["events", "messages", "deliveries", "agents", "holds"]
        .into_iter()
        .map(|t| (t, count(home, t)))
        .collect()
}

/// A spoke restored from an old copy, or a reset cursor: both start over from zero.
fn reset_cursors(s: &Spoke) {
    rusqlite::Connection::open(s.home.join(DB_FILE))
        .unwrap()
        .execute("UPDATE peers SET push_cursor = 0, pull_cursor = 0", [])
        .unwrap();
}

#[test]
fn a_hub_restart_mid_traffic_loses_nothing_and_a_replay_changes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let mut hub = Hub::start(root.path());
    let alpha = spoke(root.path(), "alpha", Clock::System);
    let beta = spoke(root.path(), "beta", Clock::System);
    hub.join(&alpha);
    hub.join(&beta);
    register(&hub.store, "hq", None);
    register(&alpha.store, "a1", None);
    register(&beta.store, "b1", None);
    sync(&alpha);
    sync(&beta);
    sync(&alpha);

    for round in 0..6 {
        send(&alpha.store, "a1", "b1@@beta", &format!("a->b {round}"));
        send(&beta.store, "b1", "a1@@alpha", &format!("b->a {round}"));
        send(&hub.store, "hq", "a1@@alpha", &format!("hq->a {round}"));
        sync(&alpha);
        if round == 2 || round == 4 {
            // Between two spokes' syncs, and with beta's push still pending.
            hub.restart();
        }
        sync(&beta);
    }
    sync(&alpha);
    sync(&beta);

    let want = |p: &str| (0..6).map(|r| format!("{p} {r}")).collect::<HashSet<_>>();
    assert_eq!(subjects(&beta.store, "b1", "a->b"), want("a->b"));
    assert_eq!(subjects(&alpha.store, "a1", "b->a"), want("b->a"));
    assert_eq!(subjects(&alpha.store, "a1", "hq->a"), want("hq->a"));
    assert_converged(&hub, &alpha);
    assert_converged(&hub, &beta);

    // Replay everything from zero: only duplicates, and no state changes anywhere.
    let before = (counts(&hub.home), counts(&alpha.home));
    reset_cursors(&alpha);
    let r = sync(&alpha);
    assert_eq!((r.pushed, r.applied), (0, 0), "{r:?}");
    assert!(r.push_duplicates > 0 && r.pull_duplicates > 0, "{r:?}");
    assert_eq!((counts(&hub.home), counts(&alpha.home)), before);
}

#[test]
fn a_spoke_offline_for_days_catches_up_in_pages() {
    let root = tempfile::tempdir().unwrap();
    let hub = Hub::start(root.path());
    let alpha = spoke(root.path(), "alpha", Clock::System);
    let start = wall_ms() - 3 * DAY_MS;
    let (beta_clock, beta_now): (Clock, Arc<AtomicI64>) = Clock::manual(start);
    let beta = spoke(root.path(), "beta", beta_clock);
    hub.join(&alpha);
    hub.join(&beta);
    register(&hub.store, "hq", None);
    register(&alpha.store, "a1", None);
    let beta_project = root.path().join("work").join("beta-app");
    register(&beta.store, "b1", Some(&beta_project));
    sync(&alpha);
    sync(&beta);
    sync(&alpha);

    // Beta is offline for three days. It keeps working: mail to alpha, and a short hold
    // each day that lapses that same day. Alpha and the hub mail beta meanwhile. Both
    // backlogs are larger than one sync page (500 events).
    const PER_DAY: usize = 220;
    for day in 0..3 {
        for i in 0..PER_DAY {
            send(&beta.store, "b1", "a1@@alpha", &format!("b->a d{day} {i}"));
            send(
                &alpha.store,
                "a1",
                "b1@beta-app@beta",
                &format!("a->b d{day} {i}"),
            );
            beta_now.fetch_add(60_000, Ordering::SeqCst);
        }
        send(
            &hub.store,
            "hq",
            "b1@beta-app@beta",
            &format!("hq->b d{day}"),
        );
        beta.store
            .acquire_holds(AcquireHolds {
                agent: "b1".into(),
                project: beta_project.to_string_lossy().into_owned(),
                paths: vec![format!("src/day{day}.rs")],
                ttl_seconds: Some(1800),
                exclusive: true,
                reason: None,
                repo_wide: false,
            })
            .unwrap();
        // Alpha keeps syncing; beta does not.
        sync(&alpha);
        beta_now.fetch_add(DAY_MS - PER_DAY as i64 * 60_000, Ordering::SeqCst);
        beta.store.sweep().unwrap();
    }
    // Back now, with one fresh hold still live.
    beta_now.store(wall_ms(), Ordering::SeqCst);
    beta.store
        .acquire_holds(AcquireHolds {
            agent: "b1".into(),
            project: beta_project.to_string_lossy().into_owned(),
            paths: vec!["src/back.rs".into()],
            ttl_seconds: Some(1800),
            exclusive: true,
            reason: None,
            repo_wide: false,
        })
        .unwrap();

    let r = sync(&beta);
    assert!(
        r.pushed > 500 && r.applied > 500,
        "both directions paged: {r:?}"
    );
    sync(&alpha);

    let want = |p: &str| {
        (0..3)
            .flat_map(|d| (0..PER_DAY).map(move |i| format!("{p} d{d} {i}")))
            .collect::<HashSet<_>>()
    };
    assert_eq!(subjects(&alpha.store, "a1", "b->a"), want("b->a"));
    assert_eq!(subjects(&beta.store, "b1", "a->b"), want("a->b"));
    assert_eq!(subjects(&beta.store, "b1", "hq->b").len(), 3);
    assert_converged(&hub, &alpha);
    assert_converged(&hub, &beta);

    // The hub and alpha project beta's holds: the days-old ones as ended, only the fresh one
    // live. (`Store::holds` lists only a node's own holds, so read the projection.)
    let now = wall_ms();
    let beta_id = beta.store.node_id().to_string();
    for (name, home) in [("hub", &hub.home), ("alpha", &alpha.home)] {
        let db = rusqlite::Connection::open(home.join(DB_FILE)).unwrap();
        let mut stmt = db
            .prepare(
                "SELECT pattern FROM holds WHERE node_id = ?1
                 AND released_at IS NULL AND expires_at > ?2 ORDER BY pattern",
            )
            .unwrap();
        let live: Vec<String> = stmt
            .query_map(rusqlite::params![beta_id, now], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(live, vec!["src/back.rs".to_string()], "{name}");
        let all: i64 = db
            .query_row(
                "SELECT count(*) FROM holds WHERE node_id = ?1",
                [&beta_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(all, 4, "{name}: every hold beta took arrived");
    }
}

#[test]
fn a_hub_restored_from_an_older_backup_converges_again() {
    restored_hub(1);
}

/// The restored hub's sequence has already passed the spoke's old pull cursor by the time
/// the spoke syncs, so "cursor ahead of the hub" alone would not reveal the restore.
#[test]
fn a_restored_hub_that_moved_past_the_old_cursor_is_still_detected() {
    restored_hub(40);
}

/// After restoring the hub from an older backup, the hub sends `hub_moves_on` messages
/// before the spoke next syncs.
fn restored_hub(hub_moves_on: usize) {
    let root = tempfile::tempdir().unwrap();
    let mut hub = Hub::start(root.path());
    let alpha = spoke(root.path(), "alpha", Clock::System);
    hub.join(&alpha);
    register(&hub.store, "hq", None);
    register(&alpha.store, "a1", None);
    sync(&alpha);
    for i in 0..3 {
        send(&alpha.store, "a1", "hq@@hub", &format!("before {i}"));
        send(&hub.store, "hq", "a1@@alpha", &format!("hq before {i}"));
    }
    sync(&alpha);

    // The operator's backup of the hub.
    let backup = root.path().join("hub-backup.db");
    rusqlite::Connection::open(hub.home.join(DB_FILE))
        .unwrap()
        .execute("VACUUM INTO ?1", [backup.to_string_lossy()])
        .unwrap();

    // Work after the backup, fully synced.
    for i in 0..3 {
        send(&alpha.store, "a1", "hq@@hub", &format!("after {i}"));
        send(&hub.store, "hq", "a1@@alpha", &format!("hq after {i}"));
    }
    sync(&alpha);
    assert_eq!(subjects(&hub.store, "hq", "after").len(), 3);

    // The hub is restored from the backup: its post-backup events are gone.
    hub.restore(&backup);
    assert!(subjects(&hub.store, "hq", "after").is_empty(), "restored");
    let lost_high = hub.store.events(1).unwrap()[0].seq;
    let (_, old_cursor) = alpha.store.push_cursor(hub.store.node_id()).unwrap();
    assert!(old_cursor > lost_high, "the spoke had seen past the backup");

    // New work on both sides after the restore.
    for i in 0..hub_moves_on {
        send(&hub.store, "hq", "a1@@alpha", &format!("hq restored {i}"));
    }
    if hub_moves_on > 1 {
        assert!(
            hub.store.events(1).unwrap()[0].seq > old_cursor,
            "the restored hub's sequence passed the spoke's old cursor"
        );
    }
    send(&alpha.store, "a1", "hq@@hub", "a restored");
    assert!(sync(&alpha).hub_reset, "the spoke noticed the restore");
    // The resync recorded an anchor the restored hub has: no reset loop.
    assert!(!sync(&alpha).hub_reset, "one resync, not one per sync");
    send(&hub.store, "hq", "a1@@alpha", "hq later");
    assert!(!sync(&alpha).hub_reset);
    assert_eq!(subjects(&alpha.store, "a1", "hq later").len(), 1);

    // Alpha still has everything it authored; the hub must get it back, and alpha must get
    // the hub's new mail.
    assert_eq!(
        subjects(&hub.store, "hq", "a restored").len(),
        1,
        "new spoke mail reaches the restored hub"
    );
    assert_eq!(
        subjects(&alpha.store, "a1", "hq restored").len(),
        hub_moves_on,
        "new hub mail reaches the spoke after the hub's sequence went backwards"
    );
    assert_eq!(
        subjects(&hub.store, "hq", "after").len(),
        3,
        "spoke events the restore lost are pushed again"
    );
    assert_converged(&hub, &alpha);
}

/// REVIEW-R1 P2: after a resync, a pull that brings only events this spoke already has (a
/// second spoke re-pushing its history) inserts nothing. The anchor must still move to the
/// last event the hub sent, or a second restore goes unnoticed and new mail is skipped.
#[test]
fn a_duplicate_only_pull_after_a_resync_still_moves_the_anchor() {
    let root = tempfile::tempdir().unwrap();
    let mut hub = Hub::start(root.path());
    let alpha = spoke(root.path(), "alpha", Clock::System);
    let beta = spoke(root.path(), "beta", Clock::System);
    hub.join(&alpha);
    hub.join(&beta);
    register(&hub.store, "hq", None);
    register(&alpha.store, "a1", None);
    register(&beta.store, "b1", None);
    for s in [&alpha, &beta, &alpha] {
        sync(s);
    }
    let backup = root.path().join("hub-backup.db");
    rusqlite::Connection::open(hub.home.join(DB_FILE))
        .unwrap()
        .execute("VACUUM INTO ?1", [backup.to_string_lossy()])
        .unwrap();

    // Beta mails alpha through the hub; alpha's anchor is now beta's mail.
    send(&beta.store, "b1", "a1@@alpha", "from beta");
    sync(&beta);
    sync(&alpha);
    assert_eq!(subjects(&alpha.store, "a1", "from beta").len(), 1);

    // First restore: alpha notices and resyncs; its anchor is older, surviving history.
    hub.restore(&backup);
    assert!(sync(&alpha).hub_reset, "first restore noticed");
    // Beta starts over and pushes its mail again; alpha gets it only as duplicates.
    reset_cursors(&beta);
    sync(&beta);
    let r = sync(&alpha);
    assert!(!r.hub_reset);
    assert!(
        r.pull_duplicates > 0 && r.applied == 0,
        "alpha had it all already: {r:?}"
    );

    // Second restore, then enough new hub mail to pass alpha's old cursor.
    hub.restore(&backup);
    for i in 0..40 {
        send(
            &hub.store,
            "hq",
            "a1@@alpha",
            &format!("second restore {i}"),
        );
    }
    assert!(sync(&alpha).hub_reset, "second restore noticed");
    assert_eq!(subjects(&alpha.store, "a1", "second restore").len(), 40);
    assert!(!sync(&alpha).hub_reset, "no reset loop");
}

/// REVIEW-R2 P2: a pull cursor must never be committed without the anchor that goes with
/// it. Here recording the anchor fails (a database error on that one insert) during the
/// duplicate-only pull. If the cursor had been committed anyway, a later restore would be
/// missed and new mail skipped.
#[test]
fn a_failed_anchor_write_does_not_leave_an_advanced_cursor() {
    let root = tempfile::tempdir().unwrap();
    let mut hub = Hub::start(root.path());
    let alpha = spoke(root.path(), "alpha", Clock::System);
    let beta = spoke(root.path(), "beta", Clock::System);
    hub.join(&alpha);
    hub.join(&beta);
    register(&hub.store, "hq", None);
    register(&alpha.store, "a1", None);
    register(&beta.store, "b1", None);
    for s in [&alpha, &beta, &alpha] {
        sync(s);
    }
    let backup = root.path().join("hub-backup.db");
    rusqlite::Connection::open(hub.home.join(DB_FILE))
        .unwrap()
        .execute("VACUUM INTO ?1", [backup.to_string_lossy()])
        .unwrap();
    send(&beta.store, "b1", "a1@@alpha", "from beta");
    sync(&beta);
    sync(&alpha);
    hub.restore(&backup);
    assert!(sync(&alpha).hub_reset);
    reset_cursors(&beta);
    sync(&beta);

    // Alpha's duplicate-only pull: the anchor write fails.
    let db = rusqlite::Connection::open(alpha.home.join(DB_FILE)).unwrap();
    db.execute_batch(
        "CREATE TRIGGER refuse_anchor BEFORE INSERT ON events WHEN NEW.kind = 'peer.anchored'
         BEGIN SELECT RAISE(ABORT, 'refused by test'); END;",
    )
    .unwrap();
    let (_, cursor_before) = alpha.store.push_cursor(hub.store.node_id()).unwrap();
    assert!(
        fed::sync(&alpha.store).is_err(),
        "the sync reports the failure"
    );
    let (_, cursor_after) = alpha.store.push_cursor(hub.store.node_id()).unwrap();
    db.execute_batch("DROP TRIGGER refuse_anchor").unwrap();
    drop(db);
    assert_eq!(
        cursor_after, cursor_before,
        "the cursor did not move without its anchor"
    );

    // A later restore and new mail: nothing is skipped.
    hub.restore(&backup);
    for i in 0..40 {
        send(&hub.store, "hq", "a1@@alpha", &format!("after failure {i}"));
    }
    sync(&alpha);
    assert_eq!(subjects(&alpha.store, "a1", "after failure").len(), 40);
}
