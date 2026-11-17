//! Adversarial (main): a very long path given to the hold and edit-guard paths.

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

fn outcome<T: std::fmt::Debug>(f: impl FnOnce() -> Result<T>) -> String {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(Ok(_)) => "ok".into(),
        Ok(Err(e)) => format!(
            "error: {}",
            e.to_string().chars().take(80).collect::<String>()
        ),
        Err(_) => "PANIC".into(),
    }
}

/// Control: the edit guard with T4's shape (many path components, up to 4 MB) does not
/// panic on `3c12e06`, so the PreEdit hook is not the way in.
#[test]
fn control_the_edit_guard_with_a_deep_huge_path_does_not_panic() {
    for reps in [100_000usize, 500_000, 2_000_000] {
        let dir = tempfile::tempdir().unwrap();
        let (store, project) = store_with(dir.path());
        let abs = format!("{project}/src/{}x.rs", "a/".repeat(reps));
        let guard =
            outcome(|| store.guard_edit("bob", &project, std::slice::from_ref(&abs), "edit"));
        eprintln!("ADV deep reps={reps} guard={guard}");
        assert!(!guard.contains("PANIC"), "reps={reps}");
    }
}

#[test]
fn gap_a_huge_path_is_refused_without_a_panic() {
    // On main `3c12e06`: 300,000 is accepted and stored; 500,000 panics.
    for len in [10_000usize, 300_000, 500_000, 1_000_000] {
        let dir = tempfile::tempdir().unwrap();
        let (store, project) = store_with(dir.path());
        let name = format!("src/{}.rs", "a".repeat(len));
        let abs = format!("{project}/{name}");
        let acquire = outcome(|| {
            store.acquire_holds(AcquireHolds {
                agent: "bob".into(),
                project: project.clone(),
                paths: vec![name.clone()],
                ttl_seconds: Some(60),
                exclusive: true,
                reason: None,
                repo_wide: false,
            })
        });
        let guard =
            outcome(|| store.guard_edit("bob", &project, std::slice::from_ref(&abs), "edit"));
        let check =
            outcome(|| store.check_holds("bob", &project, std::slice::from_ref(&name), true));
        eprintln!("ADV len={len} acquire={acquire} guard={guard} check={check}");
        assert!(
            !acquire.contains("PANIC") && !guard.contains("PANIC") && !check.contains("PANIC"),
            "len={len}"
        );
    }
}
