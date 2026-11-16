//! A held path or glob is at most `MAX_HOLD_PATH_BYTES` (queue item 48, FINDINGS-main-r2 T4
//! and FINDINGS-main-huge-path). Longer ones are refused with a short `invalid` error before
//! globset sees them; at the limit, even the costliest glob shapes compile or err, never panic.

use another_core::*;

fn store_with(dir: &std::path::Path) -> (Store, String) {
    let store = Store::open(&dir.join("home")).unwrap();
    let project = store
        .ensure_project("t", &dir.join("shop").to_string_lossy())
        .unwrap()
        .abs_path;
    store
        .register_agent(RegisterAgent {
            name: "bob".into(),
            program: Program::Other,
            project: Some(project.clone()),
            model: None,
            description: None,
        })
        .unwrap();
    (store, project)
}

fn acquire(store: &Store, project: &str, path: &str) -> Result<HoldReport> {
    store.acquire_holds(AcquireHolds {
        agent: "bob".into(),
        project: project.into(),
        paths: vec![path.into()],
        ttl_seconds: Some(600),
        exclusive: true,
        reason: None,
        repo_wide: false,
    })
}

/// `body` padded with `filler` to exactly `MAX_HOLD_PATH_BYTES` bytes.
fn at_limit(filler: &str) -> String {
    let mut p = String::from("src/");
    while p.len() + filler.len() <= MAX_HOLD_PATH_BYTES {
        p.push_str(filler);
    }
    while p.len() < MAX_HOLD_PATH_BYTES {
        p.push('a');
    }
    assert_eq!(p.len(), MAX_HOLD_PATH_BYTES);
    p
}

#[test]
fn one_byte_over_the_limit_is_refused_briefly_everywhere() {
    let dir = tempfile::tempdir().unwrap();
    let (store, project) = store_with(dir.path());
    let over = format!("{}a", at_limit("a"));
    let errors = [
        acquire(&store, &project, &over).map(|_| ()).unwrap_err(),
        store
            .check_holds("bob", &project, std::slice::from_ref(&over), true)
            .map(|_| ())
            .unwrap_err(),
        store
            .guard_edit("bob", &project, std::slice::from_ref(&over), "edit")
            .map(|_| ())
            .unwrap_err(),
    ];
    for e in errors {
        assert_eq!(e.code(), "invalid", "{e}");
        let msg = e.to_string();
        assert!(msg.contains("longer than 4096 bytes"), "{msg}");
        // The path is not echoed back.
        assert!(msg.len() < 200, "{} bytes", msg.len());
    }
    // An absolute path counts as given, before the project prefix is taken off.
    let abs = format!("{project}/{}", at_limit("a"));
    assert_eq!(
        acquire(&store, &project, &abs).unwrap_err().code(),
        "invalid"
    );
    assert!(store
        .holds(HoldFilter {
            project: None,
            agent: None,
        })
        .unwrap()
        .is_empty());
}

#[test]
fn the_costliest_globs_at_the_limit_never_panic() {
    for filler in [
        "a",
        "*",
        "**/",
        "?",
        "[a-z]",
        "{a,b}",
        "{a,{b,{c,d}}}",
        "[!a]*",
        "a/",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (store, project) = store_with(dir.path());
        let path = at_limit(filler);
        let got = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            acquire(&store, &project, &path).map(|r| r.granted.len())
        }));
        match got {
            Ok(Ok(granted)) => assert_eq!(granted, 1, "{filler:?}"),
            Ok(Err(e)) => assert_eq!(e.code(), "invalid", "{filler:?}: {e}"),
            Err(_) => panic!("{filler:?} at {MAX_HOLD_PATH_BYTES} bytes panicked"),
        }
    }
}
