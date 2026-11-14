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
    let dir = tempfile::tempdir().unwrap();
    let path = |relative: &str| dir.path().join(relative).to_string_lossy().into_owned();
    let glob = |relative: &str| path(relative).replace('\\', "/");
    s.ensure_project("t", &path("private/data")).unwrap();
    let sandbox_glob = glob("sandbox/w/**");
    let sandbox = scope(&[&sandbox_glob]);

    assert!(!s.project_allowed(&sandbox, &path("private/data")).unwrap());
    assert!(
        !s.project_allowed(&sandbox, &path("sandbox/w/../../private/data"))
            .unwrap(),
        "`..` reaches a project the globs exclude"
    );
    assert!(
        !s.project_allowed(&sandbox, &path("sandbox/w/../../brand/new"))
            .unwrap(),
        "`..` reaches a new project the globs exclude"
    );
    assert!(
        !s.project_allowed(&scope(&[&glob("work/*")]), &path("work/.."))
            .unwrap(),
        "`*` matches `..`, which leaves the work directory"
    );
    let root = dir.path().ancestors().last().unwrap();
    let root_work = root.join("work");
    let root_glob = format!("{}/*", root_work.to_string_lossy().replace('\\', "/"));
    let root_traversal = root_work.join("..");
    assert!(
        !s.project_allowed(&scope(&[&root_glob]), root_traversal.to_str().unwrap())
            .unwrap(),
        "`*` matches `..`, which is the filesystem root"
    );
    // A path that stays inside after normalizing is still fine.
    assert!(s
        .project_allowed(&sandbox, &path("sandbox/w/x/../y"))
        .unwrap());
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

/// BH-5. Durations were multiplied to milliseconds unchecked: a panic in debug builds and a
/// wrapped, wrong deadline in release builds.
#[test]
fn bh5_absurd_durations_are_refused() {
    let (clock, _) = Clock::manual(1_795_219_200_000);
    let s = Store::open_in_memory(clock).unwrap();
    for name in ["a", "b"] {
        s.register_agent(RegisterAgent {
            name: name.into(),
            program: Program::Claude,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    }
    let invalid = |r: Result<()>, what: &str| match r {
        Err(Error::Invalid(_)) => {}
        other => panic!("{what}: expected Invalid, got {other:?}"),
    };
    for secs in [i64::MAX, i64::MAX / 1000, 9_223_372_036_854_775] {
        invalid(
            s.create_token("a", &[], None, Some(secs)).map(|_| ()),
            "token expiry",
        );
        invalid(
            s.send_message(SendMessage {
                from: "a".into(),
                to: vec!["b".into()],
                subject: "s".into(),
                request: Some(NewRequest {
                    due_in_seconds: Some(secs),
                }),
                ..Default::default()
            })
            .map(|_| ()),
            "request due",
        );
    }
    // Sane values still work, up to a century out.
    s.create_token("a", &[], None, Some(100 * 365 * 86_400))
        .unwrap();
    let m = s
        .send_message(SendMessage {
            from: "a".into(),
            to: vec!["b".into()],
            subject: "s".into(),
            request: Some(NewRequest {
                due_in_seconds: Some(3600),
            }),
            ..Default::default()
        })
        .unwrap();
    let id = m.request_id.expect("request opened");
    invalid(
        s.update_request(RequestUpdate {
            agent: "b".into(),
            request_id: id,
            action: RequestAction::Claim,
            note: None,
            due_in_seconds: Some(i64::MAX),
        })
        .map(|_| ()),
        "claim due",
    );
    s.set_node_name("hub").unwrap();
    invalid(
        s.create_invite("https://hub.example", &[], i64::MAX)
            .map(|_| ()),
        "invite expiry",
    );
}
