//! Renewing and releasing holds by id (`another hold renew` / `release`, MCP `hold_paths`
//! renew and release): only the caller's own live holds, all or nothing, and for a remote
//! caller only holds in projects its token may see.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use another_core::*;

const T0: i64 = 1_800_000_000_000;

struct Fx {
    _dir: tempfile::TempDir,
    store: Store,
    clock: Arc<AtomicI64>,
    shop: String,
    blog: String,
}

impl Fx {
    fn new() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let (clock, handle) = Clock::manual(T0);
        let store = Store::open_in_memory(clock).unwrap();
        let mut paths = Vec::new();
        for name in ["shop", "blog"] {
            let p = dir.path().join(name).to_string_lossy().into_owned();
            paths.push(store.ensure_project("t", &p).unwrap().abs_path);
        }
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
        let blog = paths.pop().unwrap();
        let shop = paths.pop().unwrap();
        Fx {
            _dir: dir,
            store,
            clock: handle,
            shop,
            blog,
        }
    }

    fn hold(&self, agent: &str, project: &str, path: &str, ttl: i64) -> Hold {
        let mut report = self
            .store
            .acquire_holds(AcquireHolds {
                agent: agent.into(),
                project: project.into(),
                paths: vec![path.into()],
                ttl_seconds: Some(ttl),
                exclusive: true,
                reason: None,
                repo_wide: false,
            })
            .unwrap();
        assert!(report.conflicts.is_empty(), "{report:?}");
        report.granted.remove(0)
    }

    fn advance_secs(&self, secs: i64) {
        self.clock.fetch_add(secs * 1000, Ordering::SeqCst);
    }

    /// The live hold's current expiry, or `None` once it is no longer live.
    fn expiry(&self, hold_id: &str) -> Option<String> {
        self.store
            .holds(HoldFilter::default())
            .unwrap()
            .into_iter()
            .find(|h| h.hold_id == hold_id)
            .map(|h| h.expires_at)
    }

    fn renewed_events(&self) -> Vec<Vec<String>> {
        self.store
            .events(10_000)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "hold.renewed")
            .map(|e| {
                e.payload["hold_ids"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap().to_string())
                    .collect()
            })
            .collect()
    }
}

fn ids(holds: &[Hold]) -> Vec<String> {
    holds.iter().map(|h| h.hold_id.clone()).collect()
}

fn scope(globs: &[&str]) -> TokenScope {
    TokenScope {
        token_id: "t".into(),
        agent_id: "a".into(),
        agent: "alice".into(),
        project_globs: globs.iter().map(|g| g.to_string()).collect(),
    }
}

#[test]
fn renewing_by_id_restarts_only_those_holds_ttl_from_now() {
    let fx = Fx::new();
    let a = fx.hold("alice", &fx.shop, "src/a.rs", 600);
    let b = fx.hold("alice", &fx.blog, "src/b.rs", 600);
    fx.advance_secs(300);

    let renewed = fx
        .store
        .renew_holds("alice", std::slice::from_ref(&a.hold_id))
        .unwrap();
    assert_eq!(ids(&renewed), std::slice::from_ref(&a.hold_id));
    // A full TTL from now, not from when it was taken.
    assert!(renewed[0].expires_at > a.expires_at, "{renewed:?}");
    assert_eq!(fx.expiry(&a.hold_id), Some(renewed[0].expires_at.clone()));
    assert_eq!(fx.expiry(&b.hold_id), Some(b.expires_at.clone()));
    assert_eq!(fx.renewed_events(), [vec![a.hold_id.clone()]]);

    // No ids: every live hold of the agent.
    fx.advance_secs(60);
    let all = fx.store.renew_holds("alice", &[]).unwrap();
    assert_eq!(ids(&all), [a.hold_id.clone(), b.hold_id.clone()]);
    assert!(fx.expiry(&b.hold_id).unwrap() > b.expires_at);
}

#[test]
fn another_agents_hold_cannot_be_renewed_or_released_and_nothing_else_changes() {
    let fx = Fx::new();
    let mine = fx.hold("alice", &fx.shop, "src/a.rs", 600);
    let theirs = fx.hold("bob", &fx.shop, "src/c.rs", 600);
    fx.advance_secs(120);

    for only in [
        vec![theirs.hold_id.clone()],
        vec![mine.hold_id.clone(), theirs.hold_id.clone()],
    ] {
        let err = fx.store.renew_holds("alice", &only).unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "{err:?}");
        let err = fx.store.release_holds("alice", &only).unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "{err:?}");
    }
    // All or nothing: alice's own hold in the mixed request was neither renewed nor
    // released, and bob's is untouched.
    assert_eq!(fx.expiry(&mine.hold_id), Some(mine.expires_at.clone()));
    assert_eq!(fx.expiry(&theirs.hold_id), Some(theirs.expires_at.clone()));
    assert!(fx.renewed_events().is_empty());
}

#[test]
fn a_lapsed_hold_is_not_revived_by_renewal() {
    let fx = Fx::new();
    let gone = fx.hold("alice", &fx.shop, "src/a.rs", 60);
    fx.advance_secs(61);
    let err = fx
        .store
        .renew_holds("alice", std::slice::from_ref(&gone.hold_id))
        .unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err:?}");
    assert_eq!(fx.expiry(&gone.hold_id), None);
    // Renewing "all" finds nothing to renew and writes no event.
    assert!(fx.store.renew_holds("alice", &[]).unwrap().is_empty());
    assert!(fx.renewed_events().is_empty());
}

#[test]
fn a_remote_caller_renews_and_releases_only_inside_its_token_projects() {
    let fx = Fx::new();
    let inside = fx.hold("alice", &fx.shop, "src/a.rs", 600);
    let outside = fx.hold("alice", &fx.blog, "src/b.rs", 600);
    let token = scope(&["shop"]);
    fx.advance_secs(120);

    // A hold outside the token's projects is unknown to it, alone or with others.
    for only in [
        vec![outside.hold_id.clone()],
        vec![inside.hold_id.clone(), outside.hold_id.clone()],
    ] {
        let err = fx
            .store
            .renew_holds_scoped("alice", &only, Some(&token))
            .unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "{err:?}");
        let err = fx
            .store
            .release_holds_scoped("alice", &only, Some(&token))
            .unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "{err:?}");
    }
    assert_eq!(fx.expiry(&inside.hold_id), Some(inside.expires_at.clone()));

    // "All" means all inside the scope.
    let renewed = fx
        .store
        .renew_holds_scoped("alice", &[], Some(&token))
        .unwrap();
    assert_eq!(ids(&renewed), std::slice::from_ref(&inside.hold_id));
    assert_eq!(
        fx.expiry(&outside.hold_id),
        Some(outside.expires_at.clone())
    );
    let released = fx
        .store
        .release_holds_scoped("alice", &[], Some(&token))
        .unwrap();
    assert_eq!(released, std::slice::from_ref(&inside.hold_id));
    assert_eq!(fx.expiry(&inside.hold_id), None);
    assert_eq!(
        fx.expiry(&outside.hold_id),
        Some(outside.expires_at.clone())
    );
}
