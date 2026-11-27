//! What a hold request refuses before it writes anything: a TTL that is zero or negative,
//! and an empty path list (for an explicit hold and for the edit guard). A positive TTL
//! below the floor is raised to it rather than refused.

use another_core::*;

struct Fx {
    _dir: tempfile::TempDir,
    store: Store,
    project: String,
}

fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("work").join("shop");
    let project = project.to_string_lossy().into_owned();
    let store = Store::open_in_memory(Clock::manual(1_800_000_000_000).0)
        .unwrap()
        .unguarded_repair();
    store.ensure_project("t", &project).unwrap();
    store
        .register_agent(RegisterAgent {
            name: "alice".into(),
            program: Program::Claude,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    Fx {
        _dir: dir,
        store,
        project,
    }
}

fn acquire(fx: &Fx, paths: &[&str], ttl: Option<i64>) -> Result<HoldReport> {
    fx.store.acquire_holds(AcquireHolds {
        agent: "alice".into(),
        project: fx.project.clone(),
        paths: paths.iter().map(|p| p.to_string()).collect(),
        ttl_seconds: ttl,
        exclusive: true,
        reason: None,
        repo_wide: false,
    })
}

fn event_count(fx: &Fx) -> usize {
    fx.store.events(10_000).unwrap().len()
}

fn holds(fx: &Fx) -> Vec<Hold> {
    fx.store.holds(HoldFilter::default()).unwrap()
}

#[test]
fn a_ttl_of_zero_or_less_is_refused_and_nothing_is_written() {
    let fx = fx();
    let before = event_count(&fx);
    for ttl in [0, -1, i64::MIN] {
        let err = acquire(&fx, &["src/a.rs"], Some(ttl)).unwrap_err();
        assert_eq!(err.code(), "invalid", "{ttl}: {err}");
        assert!(
            err.to_string().contains("ttl_seconds must be positive"),
            "{ttl}: {err}"
        );
    }
    assert!(holds(&fx).is_empty());
    assert_eq!(event_count(&fx), before, "a refused hold writes no event");
}

#[test]
fn the_smallest_positive_ttl_is_raised_to_the_floor() {
    let fx = fx();
    let report = acquire(&fx, &["src/a.rs"], Some(1)).unwrap();
    assert_eq!(report.granted.len(), 1);
    assert_eq!(report.granted[0].ttl_seconds, MIN_HOLD_TTL_SECONDS);
    let report = acquire(&fx, &["src/b.rs"], None).unwrap();
    assert_eq!(report.granted[0].ttl_seconds, DEFAULT_HOLD_TTL_SECONDS);
}

#[test]
fn an_empty_path_list_is_refused_by_a_hold_and_by_the_edit_guard() {
    let fx = fx();
    let before = event_count(&fx);
    let err = acquire(&fx, &[], None).unwrap_err();
    assert_eq!(err.code(), "invalid", "{err}");
    assert!(err.to_string().contains("no paths given"), "{err}");

    let err = fx
        .store
        .guard_edit("alice", &fx.project, &[], "edit")
        .unwrap_err();
    assert_eq!(err.code(), "invalid", "{err}");
    assert!(err.to_string().contains("no paths given"), "{err}");

    assert!(holds(&fx).is_empty());
    assert_eq!(
        event_count(&fx),
        before,
        "a refused request writes no event"
    );
}
