//! When the dashboard may release someone else's hold: only when the owner is away.
//!
//! `away` is true for an offline owner, and for an idle owner quiet for at least 30 minutes
//! (`AWAY_MS`). The existing dashboard test covers a live owner (not releasable) and one idle
//! for 40 minutes (releasable). It does not cover an owner who is idle but quiet for less than
//! 30 minutes, past the 10-minute live window. That owner must keep their holds.

use std::sync::atomic::Ordering;

use another_core::*;

const T0: i64 = 1_800_000_000_000;
const MIN: i64 = 60_000;

fn owner_with_a_hold(name: &str) -> (Store, std::sync::Arc<std::sync::atomic::AtomicI64>, String) {
    let dir = tempfile::tempdir().unwrap();
    let shop = dir.path().join("work/shop").to_string_lossy().into_owned();
    let (clock, handle) = Clock::manual(T0);
    let store = Store::open_in_memory(clock).unwrap();
    store
        .register_agent(RegisterAgent {
            name: name.into(),
            program: Program::Codex,
            project: Some(shop.clone()),
            model: None,
            description: None,
        })
        .unwrap();
    let held = store
        .acquire_holds(AcquireHolds {
            agent: name.into(),
            project: shop,
            paths: vec!["src/**".into()],
            ttl_seconds: Some(4 * 3_600),
            exclusive: true,
            reason: None,
            repo_wide: false,
        })
        .unwrap();
    assert_eq!(held.granted.len(), 1);
    let hold_id = held.granted[0].hold_id.clone();
    (store, handle, hold_id)
}

fn shown(store: &Store, hold_id: &str) -> (String, bool) {
    let snap = store.dashboard_snapshot(DashboardView::LOCAL).unwrap();
    let hold = snap
        .holds
        .iter()
        .find(|h| h.hold.hold_id == hold_id)
        .expect("the hold is on the dashboard");
    let presence = snap.agents[0].presence.clone();
    (presence, hold.releasable)
}

#[test]
fn an_idle_owner_keeps_their_holds_until_thirty_quiet_minutes() {
    let (store, clock, hold_id) = owner_with_a_hold("bob");

    // 12 minutes quiet: past the live window, so idle, but not away.
    clock.store(T0 + 12 * MIN, Ordering::SeqCst);
    assert_eq!(shown(&store, &hold_id), ("idle".into(), false));
    let err = store.release_hold_from_dashboard(&hold_id).unwrap_err();
    assert_eq!(err.code(), "conflict", "{err}");

    // One millisecond short of 30 minutes: still kept.
    clock.store(T0 + 30 * MIN - 1, Ordering::SeqCst);
    assert_eq!(shown(&store, &hold_id), ("idle".into(), false));
    let err = store.release_hold_from_dashboard(&hold_id).unwrap_err();
    assert_eq!(err.code(), "conflict", "{err}");

    // At 30 minutes the owner is away: offered, and the release goes through.
    clock.store(T0 + 30 * MIN, Ordering::SeqCst);
    assert_eq!(shown(&store, &hold_id), ("idle".into(), true));
    store.release_hold_from_dashboard(&hold_id).unwrap();
    assert!(store
        .holds(HoldFilter::default())
        .unwrap()
        .iter()
        .all(|h| h.hold_id != hold_id));
}
