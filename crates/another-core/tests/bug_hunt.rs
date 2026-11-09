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
