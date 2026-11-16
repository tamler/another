//! Repository links from relative `gitdir:` files.
//!
//! The in-file test in `repo.rs` covers a linked worktree whose `.git` file names its
//! git directory by an absolute path. Git also writes relative ones:
//! `git worktree add --relative-paths` (or `worktree.useRelativePaths`) writes
//! `gitdir: ../main/.git/worktrees/<name>`, and a submodule's `.git` file is
//! `gitdir: ../.git/modules/<name>`. The layouts below copy what git 2.54 writes.
//! No `git` process runs, matching `link_for`, which only reads files.

use std::path::Path;

use another_core::repo_link_for;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// `main` (a normal checkout with `origin`) plus `wt-a`, a worktree added with
/// `--relative-paths`.
fn relative_worktree(root: &Path, origin: Option<&str>) {
    let config = match origin {
        Some(url) => format!("[remote \"origin\"]\n\turl = {url}\n"),
        None => "[core]\n\tbare = false\n".to_string(),
    };
    write(&root.join("main/.git/config"), &config);
    write(
        &root.join("wt-a/.git"),
        "gitdir: ../main/.git/worktrees/wt-a\n",
    );
    write(&root.join("main/.git/worktrees/wt-a/commondir"), "../..\n");
    write(
        &root.join("main/.git/worktrees/wt-a/gitdir"),
        "../../../../wt-a/.git\n",
    );
    std::fs::create_dir_all(root.join("wt-a/src/deep")).unwrap();
}

#[test]
fn a_worktree_with_a_relative_gitdir_shares_the_checkouts_key() {
    let dir = tempfile::tempdir().unwrap();
    relative_worktree(dir.path(), Some("git@example.com:me/shop.git"));

    let main = repo_link_for(&dir.path().join("main")).expect("main is a repository");
    let wt = repo_link_for(&dir.path().join("wt-a")).expect("the worktree is a repository");
    assert_eq!(main.key, "example.com/me/shop");
    assert_eq!(
        wt, main,
        "both checkouts of one repository share key and prefix"
    );

    let deep = repo_link_for(&dir.path().join("wt-a/src/deep")).unwrap();
    assert_eq!(deep.key, main.key);
    assert_eq!(deep.prefix, "src/deep");
}

#[test]
fn a_relative_worktree_without_origin_keys_on_the_shared_git_dir() {
    let dir = tempfile::tempdir().unwrap();
    relative_worktree(dir.path(), None);

    let main = repo_link_for(&dir.path().join("main")).unwrap();
    let wt = repo_link_for(&dir.path().join("wt-a")).unwrap();
    let shared = std::fs::canonicalize(dir.path().join("main/.git")).unwrap();
    assert_eq!(main.key, format!("git:{}", shared.to_string_lossy()));
    assert_eq!(
        wt.key, main.key,
        "the relative gitdir and commondir resolve to main's .git, not a path under wt-a"
    );
}

#[test]
fn a_submodule_is_its_own_repository() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main");
    write(
        &main.join(".git/config"),
        "[remote \"origin\"]\n\turl = https://example.com/me/shop.git\n",
    );
    write(
        &main.join("vendor/lib/.git"),
        "gitdir: ../../.git/modules/lib\n",
    );
    write(
        &main.join(".git/modules/lib/config"),
        "[core]\n\tworktree = ../../../vendor/lib\n[remote \"origin\"]\n\turl = https://example.com/them/lib.git\n",
    );
    std::fs::create_dir_all(main.join("vendor/lib/src")).unwrap();

    let outer = repo_link_for(&main.join("vendor")).unwrap();
    assert_eq!(outer.key, "example.com/me/shop");
    assert_eq!(outer.prefix, "vendor");

    let inner = repo_link_for(&main.join("vendor/lib/src")).unwrap();
    assert_eq!(
        inner.key, "example.com/them/lib",
        "the submodule's own origin, read through its relative gitdir"
    );
    assert_eq!(
        inner.prefix, "src",
        "relative to the submodule's top, not main's"
    );
}

#[test]
fn a_submodule_without_origin_does_not_take_the_superprojects_key() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main");
    write(&main.join(".git/config"), "[core]\n");
    write(&main.join("lib/.git"), "gitdir: ../.git/modules/lib\n");
    write(&main.join(".git/modules/lib/config"), "[core]\n");

    let outer = repo_link_for(&main).unwrap();
    let inner = repo_link_for(&main.join("lib")).unwrap();
    let modules = std::fs::canonicalize(main.join(".git/modules/lib")).unwrap();
    assert_eq!(inner.key, format!("git:{}", modules.to_string_lossy()));
    assert_ne!(inner.key, outer.key);
}

/// adv237: a relative-path worktree opened through a symlink whose parent is not the
/// worktree's real parent. Git resolves `gitdir: ../main/...` from the `.git` file's real
/// location, so the worktree still belongs to `main`; a lexical join from the symlinked path
/// lands next to the symlink instead.
#[cfg(unix)]
#[test]
fn adv237_a_symlinked_relative_worktree_still_shares_the_checkouts_key() {
    let dir = tempfile::tempdir().unwrap();
    relative_worktree(dir.path(), Some("git@example.com:me/shop.git"));
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::os::unix::fs::symlink(dir.path().join("wt-a"), elsewhere.join("wt")).unwrap();

    let main = repo_link_for(&dir.path().join("main")).unwrap();
    let via_link =
        repo_link_for(&elsewhere.join("wt")).expect("the symlinked worktree is still a repository");
    assert_eq!(
        via_link.key, main.key,
        "opened through a symlink, the worktree got a key of its own"
    );
}

/// A worktree whose link was cached under the old key rules (KEY_VERSION v2, from before the
/// symlink fix) is relinked, not trusted: its holds count against the main checkout's.
#[cfg(unix)]
#[test]
fn a_link_cached_under_the_old_key_rules_is_recomputed() {
    use another_core::*;
    let dir = tempfile::tempdir().unwrap();
    relative_worktree(dir.path(), Some("git@example.com:me/shop.git"));
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::os::unix::fs::symlink(dir.path().join("wt-a"), elsewhere.join("wt")).unwrap();
    let home = dir.path().join("home");
    let store = Store::open(&home).unwrap();
    let main = store
        .ensure_project("t", &dir.path().join("main").to_string_lossy())
        .unwrap();
    let wt = store
        .ensure_project("t", &elsewhere.join("wt").to_string_lossy())
        .unwrap();
    for (name, project) in [("alice", &main), ("bob", &wt)] {
        store
            .register_agent(RegisterAgent {
                name: name.into(),
                program: Program::Other,
                project: Some(project.abs_path.clone()),
                model: None,
                description: None,
            })
            .unwrap();
    }
    // What the pre-fix build cached for the symlinked worktree: a key of its own.
    rusqlite::Connection::open(home.join(DB_FILE))
        .unwrap()
        .execute(
            "UPDATE project_repos SET repo_key = 'v2:git:/nowhere/.git', repo_prefix = ''
             WHERE project_id = ?1",
            [&wt.project_id],
        )
        .unwrap();
    let hold = |agent: &str, project: &Project| {
        store
            .acquire_holds(AcquireHolds {
                agent: format!("{agent}@{}", project.slug),
                project: project.abs_path.clone(),
                paths: vec!["src/a.rs".into()],
                ttl_seconds: Some(600),
                exclusive: true,
                reason: None,
                repo_wide: false,
            })
            .unwrap()
    };
    assert_eq!(hold("alice", &main).granted.len(), 1);
    let bob = hold("bob", &wt);
    assert!(
        bob.granted.is_empty(),
        "the stale v2 key was trusted: {bob:?}"
    );
    assert_eq!(bob.conflicts.len(), 1, "{bob:?}");
}
