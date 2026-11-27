//! A new file named in another letter case conflicts with a hold only where the volume
//! folds case. The volume is probed through the project directory's own name; when that
//! name has no letters (`2026/11`), the probe respells the nearest ancestor that has
//! letters and walks back down. These tests run that walk.

use std::path::Path;

use another_core::*;

/// Ground truth for the volume holding `dir`: write one name, look it up in another case.
fn volume_folds_case(dir: &Path) -> bool {
    let probe = dir.join("case-probe");
    std::fs::write(&probe, b"").unwrap();
    let folds = dir.join("CASE-PROBE").exists();
    std::fs::remove_file(probe).unwrap();
    folds
}

fn register(store: &Store, name: &str) {
    store
        .register_agent(RegisterAgent {
            name: name.into(),
            program: Program::Claude,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
}

fn hold(store: &Store, agent: &str, project: &str, path: &str) -> HoldReport {
    store
        .acquire_holds(AcquireHolds {
            agent: agent.into(),
            project: project.into(),
            paths: vec![path.into()],
            ttl_seconds: None,
            exclusive: true,
            reason: None,
            repo_wide: false,
        })
        .unwrap()
}

/// `rel` is the project directory below the temp dir; every name in it has no letters,
/// so the probe climbs `rel.len()` levels to the `Work` directory before it can respell.
fn conflict_in(rel: &[&str]) -> (bool, bool) {
    let dir = tempfile::tempdir().unwrap();
    let mut project = dir.path().join("Work");
    for part in rel {
        project.push(part);
    }
    std::fs::create_dir_all(&project).unwrap();
    let folds = volume_folds_case(&project);
    let path = project.to_string_lossy().into_owned();
    let store = Store::open_in_memory(Clock::manual(1_800_000_000_000).0)
        .unwrap()
        .unguarded_repair();
    store.ensure_project("t", &path).unwrap();
    register(&store, "alice");
    register(&store, "bob");
    assert!(hold(&store, "alice", &path, "src/new.rs")
        .conflicts
        .is_empty());
    let report = hold(&store, "bob", &path, "src/NEW.RS");
    (folds, !report.conflicts.is_empty())
}

#[test]
fn a_project_directory_without_letters_still_finds_the_volume_case_rule() {
    let (folds, conflict) = conflict_in(&["2026"]);
    assert_eq!(
        conflict, folds,
        "src/NEW.RS should conflict with src/new.rs exactly when the volume folds case"
    );
}

#[test]
fn the_probe_walks_back_down_through_several_names_without_letters() {
    let (folds, conflict) = conflict_in(&["2026", "11", "17"]);
    assert_eq!(
        conflict, folds,
        "src/NEW.RS should conflict with src/new.rs exactly when the volume folds case"
    );
}

#[test]
fn a_different_name_never_conflicts_on_any_volume() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("Work").join("2026");
    std::fs::create_dir_all(&project).unwrap();
    let path = project.to_string_lossy().into_owned();
    let store = Store::open_in_memory(Clock::manual(1_800_000_000_000).0)
        .unwrap()
        .unguarded_repair();
    store.ensure_project("t", &path).unwrap();
    register(&store, "alice");
    register(&store, "bob");
    assert!(hold(&store, "alice", &path, "src/new.rs")
        .conflicts
        .is_empty());
    assert!(hold(&store, "bob", &path, "src/NEW2.RS")
        .conflicts
        .is_empty());
}
