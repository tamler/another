//! Regression tests from the v0.1.0 bug hunt (`another-bug-hunt-BUGS.md`), one per finding,
//! named after its bug number.

use another_core::*;

fn scope(globs: &[&str]) -> TokenScope {
    TokenScope {
        token_id: "t".into(),
        agent_id: "a".into(),
        agent: "a".into(),
        project_globs: globs.iter().map(|g| g.to_string()).collect(),
    }
}

/// BH-1. The glob is matched against the raw string, but every later lookup normalizes it.
#[test]
fn bh1_project_allowed_judges_the_path_the_store_will_use() {
    let (clock, _) = Clock::manual(1_795_219_200_000);
    let s = Store::open_in_memory(clock).unwrap();
    s.ensure_project("t", "/private/data").unwrap();
    let sandbox = scope(&["/sandbox/w/**"]);

    assert!(!s.project_allowed(&sandbox, "/private/data").unwrap());
    assert!(
        !s.project_allowed(&sandbox, "/sandbox/w/../../private/data")
            .unwrap(),
        "`..` reaches a project the globs exclude"
    );
    assert!(
        !s.project_allowed(&sandbox, "/sandbox/w/../../brand/new")
            .unwrap(),
        "`..` reaches a new project the globs exclude"
    );
    assert!(
        !s.project_allowed(&scope(&["/work/*"]), "/work/..").unwrap(),
        "`*` matches `..`, which is the filesystem root"
    );
    // A path that stays inside after normalizing is still fine.
    assert!(s.project_allowed(&sandbox, "/sandbox/w/x/../y").unwrap());
}

/// BH-2. Several processes opening a new home at once each ran the migrations, because the
/// schema version was read once, before waiting for the write lock.
#[test]
fn bh2_concurrent_first_open_succeeds() {
    use std::sync::{Arc, Barrier};
    const OPENERS: usize = 8;
    for round in 0..10 {
        let home = tempfile::tempdir().unwrap();
        let barrier = Arc::new(Barrier::new(OPENERS));
        let handles: Vec<_> = (0..OPENERS)
            .map(|_| {
                let path = home.path().to_path_buf();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    Store::open(&path).map(|s| s.node_id().to_string())
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let errors: Vec<String> = results
            .iter()
            .filter_map(|r| r.as_ref().err().map(|e| e.to_string()))
            .collect();
        assert!(errors.is_empty(), "round {round}: {errors:?}");
        let ids: std::collections::BTreeSet<_> = results.into_iter().map(Result::unwrap).collect();
        assert_eq!(ids.len(), 1, "round {round}: one local node, got {ids:?}");
    }
}
