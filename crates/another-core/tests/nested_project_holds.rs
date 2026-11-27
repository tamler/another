//! A file inside a project nested in another project (outside git) is one file, so an
//! exclusive hold taken through either project must stop the other. Inside a git
//! repository, checkout identity already relates projects of that checkout (D92); this
//! covers plain directories such as a shared notes or lanes folder.

use another_core::{AcquireHolds, Clock, Program, RegisterAgent, Store};

fn setup(git: bool) -> (tempfile::TempDir, Store, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("work");
    std::fs::create_dir_all(parent.join("sub")).unwrap();
    if git {
        std::fs::create_dir_all(parent.join(".git")).unwrap();
    }
    std::fs::write(parent.join("sub/f.md"), "x\n").unwrap();
    let store = Store::open_in_memory(Clock::System).unwrap();
    for name in ["alpha", "beta"] {
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
    let parent_p = store
        .ensure_project("test", parent.to_str().unwrap())
        .unwrap()
        .abs_path;
    let child_p = store
        .ensure_project("test", parent.join("sub").to_str().unwrap())
        .unwrap()
        .abs_path;
    (dir, store, parent_p, child_p)
}

fn hold(store: &Store, agent: &str, project: &str, path: &str) -> usize {
    store
        .acquire_holds(AcquireHolds {
            agent: agent.into(),
            project: project.into(),
            paths: vec![path.into()],
            ttl_seconds: Some(600),
            exclusive: true,
            reason: None,
            repo_wide: false,
        })
        .unwrap()
        .granted
        .len()
}

#[test]
fn inside_git_a_parent_and_child_project_share_one_hold() {
    let (_d, store, parent, child) = setup(true);
    assert_eq!(hold(&store, "alpha", &parent, "sub/f.md"), 1);
    assert_eq!(
        hold(&store, "beta", &child, "f.md"),
        0,
        "control: git relates them"
    );
}

#[test]
fn outside_git_a_parent_and_child_project_share_one_hold() {
    let (_d, store, parent, child) = setup(false);
    assert_eq!(hold(&store, "alpha", &parent, "sub/f.md"), 1);
    assert_eq!(
        hold(&store, "beta", &child, "f.md"),
        0,
        "beta got an exclusive hold on the file alpha already holds exclusively"
    );
}

#[test]
fn outside_git_the_edit_guard_in_a_child_project_sees_the_parent_hold() {
    let (_d, store, parent, child) = setup(false);
    assert_eq!(hold(&store, "alpha", &parent, "sub/f.md"), 1);
    let guard = store
        .guard_edit("beta", &child, &["f.md".into()], "editing (auto)")
        .unwrap();
    assert_eq!(guard.conflicts.len(), 1, "the edit guard must stop beta");
}

fn plain_projects(dirs: &[&str]) -> (tempfile::TempDir, Store, Vec<String>) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory(Clock::System).unwrap();
    for name in ["alpha", "beta"] {
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
    let projects = dirs
        .iter()
        .map(|d| {
            let path = dir.path().join(d);
            std::fs::create_dir_all(&path).unwrap();
            store
                .ensure_project("test", path.to_str().unwrap())
                .unwrap()
                .abs_path
        })
        .collect();
    (dir, store, projects)
}

#[test]
fn outside_git_a_child_hold_stops_the_parent_too() {
    let (_d, store, parent, child) = setup(false);
    assert_eq!(hold(&store, "beta", &child, "f.md"), 1);
    assert_eq!(
        hold(&store, "alpha", &parent, "sub/f.md"),
        0,
        "the child's hold covers the parent's path"
    );
    // A glob in the parent that reaches into the child conflicts as well.
    assert_eq!(hold(&store, "alpha", &parent, "sub/*.md"), 0);
    // Another file in the parent does not.
    assert_eq!(hold(&store, "alpha", &parent, "other.md"), 1);
}

#[test]
fn sibling_and_prefix_named_directories_do_not_conflict() {
    // `work/a` and `work/ab` share a string prefix but neither contains the other.
    let (_d, store, p) = plain_projects(&["work/a", "work/ab", "work/b"]);
    assert_eq!(hold(&store, "alpha", &p[0], "f.md"), 1);
    assert_eq!(hold(&store, "beta", &p[1], "f.md"), 1);
    assert_eq!(hold(&store, "beta", &p[2], "f.md"), 1);
}

#[test]
fn a_child_directory_with_glob_characters_stays_literal() {
    // A child project at `work/app/[id]`: the parent's `app/x/f.md` is not inside it, even
    // though the glob `app/[id]/f.md` would match `app/i/f.md`.
    let (_d, store, p) = plain_projects(&["work", "work/app/[id]", "work/app/i"]);
    assert_eq!(hold(&store, "beta", &p[1], "f.md"), 1);
    assert_eq!(
        hold(&store, "alpha", &p[0], "app/i/f.md"),
        1,
        "`[id]` is a directory name, not a character class"
    );
    assert_eq!(
        hold(&store, "alpha", &p[0], "app/[[]id[]]/f.md"),
        0,
        "the same file through the parent"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn nested_projects_compare_case_insensitively_where_the_volume_folds_case() {
    let (_d, store, p) = plain_projects(&["work", "work/Sub"]);
    // The parent names the child directory in another letter case.
    assert_eq!(hold(&store, "beta", &p[1], "f.md"), 1);
    assert_eq!(hold(&store, "alpha", &p[0], "sub/f.md"), 0);
}

/// Nesting compares directories, and a directory names a place only on its own machine: a
/// paired node's project at a path below (or above) a local project is not the same files,
/// so its holds do not stop local work there. Local Git conflicts use checkout identity;
/// holds on other machines that share a project slug are warnings, never conflicts.
#[test]
fn a_paired_nodes_project_at_a_nested_path_is_not_related() {
    use another_core::{Perm, DB_FILE};

    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(work.join("sub")).unwrap();
    let open = |name: &str| {
        let store = Store::open(&dir.path().join(name)).unwrap();
        store.set_node_name(name).unwrap();
        store
    };
    let hub = open("hub");
    let spoke = open("spoke");
    let code = hub
        .create_invite("https://hub.example", &Perm::ALL, 3600)
        .unwrap();
    let (invite, req) = spoke.prepare_join(&code).unwrap();
    let resp = hub.accept_pairing(&req).unwrap();
    spoke.complete_join(&invite, &resp).unwrap();
    for (store, name) in [(&hub, "alpha"), (&spoke, "beta")] {
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

    // The spoke holds sub/f.md in its project at work/sub, and the hub learns of it.
    let remote_child = spoke
        .ensure_project("test", work.join("sub").to_str().unwrap())
        .unwrap()
        .abs_path;
    assert_eq!(hold(&spoke, "beta", &remote_child, "f.md"), 1);
    let (events, _) = spoke.outbound_events(0, 10_000).unwrap();
    let report = hub
        .ingest(spoke.node_id(), &events, &[spoke.node_entry().unwrap()])
        .unwrap();
    assert!(report.rejected.is_empty(), "{report:?}");
    let synced: i64 = rusqlite::Connection::open(dir.path().join("hub").join(DB_FILE))
        .unwrap()
        .query_row(
            "SELECT count(*) FROM holds WHERE node_id = ?1 AND released_at IS NULL",
            [spoke.node_id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(synced, 1, "the hub has the spoke's hold");

    // The hub's own project at work, and the same relative file through it.
    let local_parent = hub
        .ensure_project("test", work.to_str().unwrap())
        .unwrap()
        .abs_path;
    assert_eq!(
        hold(&hub, "alpha", &local_parent, "sub/f.md"),
        1,
        "a path on another machine is not a nested project here"
    );
}
