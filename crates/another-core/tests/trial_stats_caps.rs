//! `trial_stats` counts every wake but lists at most 50 of them, newest first, so a busy
//! trial cannot make the report unbounded. The counts must stay exact past the cap.

use another_core::*;

#[test]
fn wakes_are_counted_in_full_but_listed_fifty_newest_first() {
    let s = Store::open_in_memory(Clock::manual(1_800_000_000_000).0).unwrap();
    s.register_agent(RegisterAgent {
        name: "alice".into(),
        program: Program::Claude,
        project: None,
        model: None,
        description: None,
    })
    .unwrap();
    for n in 0..60 {
        let outcome = if n % 10 == 0 { "failed" } else { "sent" };
        s.record_wake(
            "alice",
            WakeVia::Herdr,
            &format!("w{n}:p1"),
            outcome,
            "",
            &[],
        )
        .unwrap();
    }

    let stats = s.trial_stats(None).unwrap();
    assert_eq!(stats.wakes_sent, 54);
    assert_eq!(stats.wakes_failed, 6);
    assert_eq!(stats.wakes.len(), 50, "the list is capped");
    let targets: Vec<&str> = stats.wakes.iter().map(|w| w.target.as_str()).collect();
    assert_eq!(targets.first(), Some(&"w59:p1"), "newest first");
    assert_eq!(
        targets.last(),
        Some(&"w10:p1"),
        "the oldest ten are left out"
    );
}

/// The same cap on blocked edits: every collision the edit guard records is counted, but only
/// the newest 50 are listed. (`guard_edit` is the core call the pre-edit hook makes.)
#[test]
fn blocked_edits_are_counted_in_full_but_listed_fifty_newest_first() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open_in_memory(Clock::manual(1_800_000_000_000).0).unwrap();
    let shop = dir.path().join("shop");
    std::fs::create_dir_all(&shop).unwrap();
    let project = s
        .ensure_project("t", &shop.to_string_lossy())
        .unwrap()
        .abs_path;
    for name in ["alice", "bob"] {
        s.register_agent(RegisterAgent {
            name: name.into(),
            program: Program::Claude,
            project: Some(project.clone()),
            model: None,
            description: None,
        })
        .unwrap();
    }
    let paths: Vec<String> = (0..60).map(|n| format!("src/f{n}.rs")).collect();
    let held = s
        .acquire_holds(AcquireHolds {
            agent: "alice@shop".into(),
            project: project.clone(),
            paths: paths.clone(),
            ttl_seconds: Some(3600),
            exclusive: true,
            reason: None,
            repo_wide: false,
        })
        .unwrap();
    assert_eq!(held.granted.len(), 60);
    for p in &paths {
        let guard = s
            .guard_edit("bob@shop", &project, std::slice::from_ref(p), "auto")
            .unwrap();
        assert_eq!(guard.conflicts.len(), 1, "{p}");
    }

    let stats = s.trial_stats(None).unwrap();
    assert_eq!(stats.edits_blocked, 60);
    assert_eq!(stats.collisions, 60);
    assert_eq!(stats.blocked.len(), 50);
    assert_eq!(stats.blocked[0].path, "src/f59.rs", "newest first");
    assert_eq!(stats.blocked[49].path, "src/f10.rs");
}
