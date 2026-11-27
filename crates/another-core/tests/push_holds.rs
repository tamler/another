//! Holds across the projects of one checkout, the edit guard, keepalive, the unread cursor
//! query, the quick hook open, and automatic wake target cleanup.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use another_core::*;

const T0: i64 = 1_800_000_000_000;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Make `link` a second name for the directory `target`: a symlink, or on Windows a
/// junction, which needs no privilege.
fn link_dir(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    {
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(made.status.success(), "mklink /J: {made:?}");
    }
}

/// A main checkout with an `origin`, a second project of that same checkout, a linked
/// worktree of it, and an unrelated repository.
struct Repos {
    _dir: tempfile::TempDir,
    home: PathBuf,
    main: PathBuf,
    /// A second project in the main checkout: the same directory under another name, as a
    /// project registered under a symlinked spelling is. Its holds and the main project's
    /// block each other (D92: one checkout), with the same paths in both.
    twin: PathBuf,
    /// A linked worktree: another checkout of the same repository. Nothing blocks across.
    wt: PathBuf,
    other: PathBuf,
}

impl Repos {
    fn new() -> Repos {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let main = root.join("shop");
        write(
            &main.join(".git/config"),
            "[remote \"origin\"]\n\turl = git@example.com:me/shop.git\n",
        );
        std::fs::create_dir_all(main.join("src")).unwrap();
        let wt = root.join("shop-wt");
        let wt_git = main.join(".git/worktrees/shop-wt");
        write(&wt.join(".git"), &format!("gitdir: {}\n", wt_git.display()));
        write(&wt_git.join("commondir"), "../..\n");
        let twin = root.join("shop-twin");
        link_dir(&main, &twin);
        let other = root.join("blog");
        write(
            &other.join(".git/config"),
            "[remote \"origin\"]\n\turl = https://example.com/me/blog\n",
        );
        Repos {
            home: root.join("home"),
            _dir: dir,
            main,
            twin,
            wt,
            other,
        }
    }

    fn path(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    fn root(&self) -> &Path {
        self._dir.path()
    }
}

struct Fx {
    repos: Repos,
    store: Store,
    clock: Arc<AtomicI64>,
}

impl Fx {
    fn new() -> Fx {
        let repos = Repos::new();
        let (clock, handle) = Clock::manual(T0);
        let store = Store::open_with_clock(&repos.home, clock)
            .unwrap()
            .unguarded_repair();
        for (name, program) in [
            ("alice", Program::Claude),
            ("bob", Program::Codex),
            ("carol", Program::Grok),
        ] {
            store
                .register_agent(RegisterAgent {
                    name: name.into(),
                    program,
                    project: None,
                    model: None,
                    description: None,
                })
                .unwrap();
        }
        Fx {
            repos,
            store,
            clock: handle,
        }
    }

    fn advance_secs(&self, secs: i64) {
        self.clock.fetch_add(secs * 1000, Ordering::SeqCst);
    }

    fn project(&self, dir: &Path) -> String {
        self.store
            .ensure_project("test", &Repos::path(dir))
            .unwrap()
            .abs_path
    }

    fn acquire(&self, agent: &str, dir: &Path, paths: &[&str]) -> Result<HoldReport> {
        self.store.acquire_holds(AcquireHolds {
            agent: agent.into(),
            project: self.project(dir),
            paths: paths.iter().map(|s| s.to_string()).collect(),
            ttl_seconds: Some(600),
            exclusive: true,
            reason: Some("work".into()),
            repo_wide: false,
        })
    }

    fn kinds(&self) -> Vec<String> {
        self.store
            .events(10_000)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect()
    }
}

/// A4 (D92). A project registered at a subdirectory of a checkout is in the same checkout
/// as the top-level project: holds block across the two, in both directions, compared by
/// paths from the checkout's top. A linked worktree of the repository is another checkout.
#[test]
fn a_subdirectory_project_and_its_checkout_are_one() {
    let fx = Fx::new();
    let r = &fx.repos;
    let sub = r.main.join("src");
    // Held through the top-level project, stopped through the project at `src`.
    let granted = fx.acquire("alice", &r.main, &["src/**"]).unwrap();
    assert_eq!(granted.granted.len(), 1);
    let clash = fx.acquire("carol", &sub, &["lib.rs"]).unwrap();
    assert_eq!(
        clash.conflicts.len(),
        1,
        "src/lib.rs is under alice's src/**"
    );
    assert_eq!(clash.conflicts[0].held.agent, "alice");
    assert_eq!(clash.conflicts[0].held.project, "shop");
    assert!(fx.acquire("carol", &sub, &["../README.md"]).is_err());
    let edit = |agent: &str, dir: &Path, file: &str| {
        fx.store
            .guard_edit(agent, &fx.project(dir), &[file.into()], "auto")
            .unwrap()
    };
    assert_eq!(edit("carol", &sub, "api/x.rs").conflicts.len(), 1);
    // And the reverse: held through the project at `src`, stopped through the top.
    fx.store.release_holds("alice", &[]).unwrap();
    assert_eq!(edit("carol", &sub, "api/x.rs").held.len(), 1);
    let stopped = edit("bob", &r.main, "src/api/x.rs");
    assert_eq!(stopped.conflicts.len(), 1);
    assert_eq!(stopped.conflicts[0].held.agent, "carol");
    assert_eq!(stopped.conflicts[0].held.project, "src");
    assert!(edit("bob", &r.main, "src/api/y.rs").conflicts.is_empty());

    // The worktree is another checkout of the same repository: nothing of the main
    // checkout applies there, for `acquire`, `check` and the edit guard.
    assert_eq!(
        fx.acquire("bob", &r.wt, &["src/api/**", "docs/x.md"])
            .unwrap()
            .granted
            .len(),
        2
    );
    let conflicts = fx
        .store
        .check_holds("alice", &fx.project(&r.wt), &["src/api/y.rs".into()], true)
        .unwrap();
    assert_eq!(conflicts.len(), 1, "bob's hold, inside the worktree");
    assert_eq!(conflicts[0].held.project, "shop-wt");
    assert!(edit("alice", &r.main, "src/api/z.rs").conflicts.is_empty());
    assert_eq!(
        fx.acquire("bob", &r.other, &["src/lib.rs"])
            .unwrap()
            .granted
            .len(),
        1
    );

    // Listing a project's holds still shows the other checkouts of its repository, for
    // anyone who looks; an unrelated repository is left out.
    let listed = fx
        .store
        .holds(HoldFilter {
            project: Some(fx.project(&r.wt)),
            agent: None,
        })
        .unwrap();
    let patterns: Vec<_> = listed
        .iter()
        .map(|h| (h.project.as_str(), h.pattern.as_str()))
        .collect();
    assert!(patterns.contains(&("src", "api/x.rs")), "{patterns:?}");
    assert!(patterns.contains(&("shop-wt", "docs/x.md")), "{patterns:?}");
    assert!(!patterns.iter().any(|(p, _)| *p == "blog"), "{patterns:?}");
}

/// A4, second half. One checkout registered under two spellings, or reached through a
/// symlinked parent directory, is one checkout: the candidate set is found by the
/// physically resolved top, not by the spelling a project was registered under.
#[cfg(unix)]
#[test]
fn one_checkout_under_two_spellings_still_blocks() {
    let fx = Fx::new();
    let r = &fx.repos;
    let root = r.root();
    // A symlink to the checkout itself, and a symlinked parent directory.
    let alias = root.join("shop-alias");
    std::os::unix::fs::symlink(&r.main, &alias).unwrap();
    let parent = root.join("linked-parent");
    std::os::unix::fs::symlink(root, &parent).unwrap();
    let through_parent = parent.join("shop");
    let mut spellings = vec![alias, through_parent];
    // The temporary directory itself usually has two spellings on macOS (`/var/...` and
    // `/private/var/...`), as `/tmp/x` and `/private/tmp/x` do.
    let real = std::fs::canonicalize(&r.main).unwrap();
    if real != r.main {
        spellings.push(real);
    }
    let edit = |agent: &str, dir: &Path, file: &str| {
        fx.store
            .guard_edit(agent, &fx.project(dir), &[file.into()], "auto")
            .unwrap()
    };
    let main_project = fx.store.project(&fx.project(&r.main)).unwrap();
    for (i, spelling) in spellings.iter().enumerate() {
        let project = fx.store.project(&fx.project(spelling)).unwrap();
        assert_ne!(
            project.project_id, main_project.project_id,
            "{spelling:?} is registered as its own project"
        );
        // Held under the main spelling, stopped under the other one, and the reverse.
        let (a, b) = (format!("src/a{i}.rs"), format!("src/b{i}.rs"));
        assert!(edit("alice", &r.main, &a).conflicts.is_empty());
        let stopped = edit("bob", spelling, &a);
        assert_eq!(stopped.conflicts.len(), 1, "{spelling:?}");
        assert_eq!(stopped.conflicts[0].held.agent, "alice");
        assert!(edit("bob", spelling, &b).conflicts.is_empty());
        assert_eq!(
            edit("alice", &r.main, &b).conflicts.len(),
            1,
            "{spelling:?}"
        );
    }
}

/// The checkout value relies on `std::fs::canonicalize` returning a directory's on-disk
/// letter case (D56 claims it for files). Where the volume ignores case, a project
/// registered under another spelling of its directory is in the same checkout.
#[test]
fn a_checkout_is_stored_in_its_on_disk_letter_case() {
    let fx = Fx::new();
    let root = fx.repos.root();
    let real = root.join("CamelCase");
    write(&real.join(".git/config"), "[core]\n");
    let other = root.join("camelcase");
    let on_disk = repo_link_for(&real).unwrap().checkout;
    assert!(on_disk.ends_with("CamelCase"), "{on_disk}");
    if std::fs::metadata(&other).is_err() {
        // A volume that tells the two names apart: nothing more to check.
        return;
    }
    assert_eq!(
        std::fs::canonicalize(&other)
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy(),
        "CamelCase",
        "canonicalize restores the on-disk case of a directory"
    );
    assert_eq!(repo_link_for(&other).unwrap().checkout, on_disk);
    let edit = |agent: &str, dir: &Path, file: &str| {
        fx.store
            .guard_edit(agent, &fx.project(dir), &[file.into()], "auto")
            .unwrap()
    };
    assert!(edit("alice", &real, "src/a.rs").conflicts.is_empty());
    assert_eq!(edit("bob", &other, "src/a.rs").conflicts.len(), 1);
}

#[test]
fn remotes_differing_only_in_path_case_are_different_repositories() {
    // Review F4: hostnames are case-insensitive, repository paths need not be.
    let fx = Fx::new();
    let upper = fx.repos.root().join("upper");
    let lower = fx.repos.root().join("lower");
    write(
        &upper.join(".git/config"),
        "[remote \"origin\"]\n\turl = git@example.invalid:Team/Shop.git\n",
    );
    write(
        &lower.join(".git/config"),
        "[remote \"origin\"]\n\turl = https://EXAMPLE.invalid/team/shop\n",
    );
    assert_eq!(
        fx.acquire("alice", &upper, &["src/a.txt"])
            .unwrap()
            .granted
            .len(),
        1
    );
    let other = fx.acquire("bob", &lower, &["src/a.txt"]).unwrap();
    assert!(other.conflicts.is_empty(), "{:?}", other.conflicts);
    // Same path case, different host case and scheme: still one repository. Clones never
    // block each other (D92), so the repository shows in what a project's listing counts
    // as its sibling checkouts.
    let same = fx.repos.root().join("same");
    write(
        &same.join(".git/config"),
        "[remote \"origin\"]\n\turl = https://EXAMPLE.invalid/Team/Shop.git\n",
    );
    assert_eq!(
        fx.acquire("carol", &same, &["src/a.txt"])
            .unwrap()
            .granted
            .len(),
        1
    );
    let siblings: Vec<String> = fx
        .store
        .holds(HoldFilter {
            project: Some(fx.project(&same)),
            agent: None,
        })
        .unwrap()
        .into_iter()
        .map(|h| h.project)
        .collect();
    assert_eq!(siblings, ["upper", "same"]);
}

#[test]
fn projects_from_before_repo_links_are_linked_on_demand() {
    let fx = Fx::new();
    let r = &fx.repos;
    fx.acquire("alice", &r.main, &["src/**"]).unwrap();
    // A second project of the same checkout: one registered at a subdirectory.
    let sub = fx.project(&r.main.join("src"));
    // Simulate a database whose projects predate the link cache.
    let db = rusqlite::Connection::open(r.home.join(DB_FILE)).unwrap();
    db.execute("DELETE FROM project_repos", []).unwrap();
    let report = fx
        .store
        .acquire_holds(AcquireHolds {
            agent: "bob".into(),
            project: sub,
            paths: vec!["a.rs".into()],
            ttl_seconds: None,
            exclusive: true,
            reason: None,
            repo_wide: false,
        })
        .unwrap();
    assert_eq!(
        report.conflicts.len(),
        1,
        "legacy hold in the top-level project still counts"
    );
    // Both projects were linked on the way, to the one checkout they share.
    let checkouts: Vec<String> = db
        .prepare("SELECT checkout FROM project_repos ORDER BY project_id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let top = resolve_existing(&r.main).to_string_lossy().into_owned();
    assert_eq!(checkouts, [top.clone(), top]);
}

#[test]
fn whole_project_holds_need_repo_wide() {
    let fx = Fx::new();
    let main = fx.repos.main.clone();
    for broad in ["**", ".", "**/*.rs", "*/**"] {
        let err = fx.acquire("alice", &main, &[broad]).unwrap_err();
        assert!(err.to_string().contains("--repo-wide"), "{broad}: {err}");
    }
    let root = Repos::path(&main);
    assert!(fx.acquire("alice", &main, &[root.as_str()]).is_err());
    // Narrow globs are fine, and the flag allows the broad ones.
    assert_eq!(
        fx.acquire("alice", &main, &["src/**"])
            .unwrap()
            .granted
            .len(),
        1
    );
    let all = fx
        .store
        .acquire_holds(AcquireHolds {
            agent: "bob".into(),
            project: fx.project(&fx.repos.other),
            paths: vec!["**".into()],
            ttl_seconds: None,
            exclusive: true,
            reason: None,
            repo_wide: true,
        })
        .unwrap();
    assert_eq!(all.granted[0].pattern, "**");
    // Checking a broad pattern takes nothing, so it stays allowed.
    assert!(fx
        .store
        .check_holds("carol", &fx.project(&main), &["**".into()], true)
        .is_ok());
}

#[test]
fn manual_globs_under_literal_route_components_conflict_in_both_orders() {
    assert!(!patterns_overlap(
        "app/[id]/*.tsx",
        &globset::escape("app/[id]/page.rs")
    ));
    for (pattern, path) in [
        ("app/[id]/*.tsx", "app/[id]/page.tsx"),
        ("app/[id]/**/*.tsx", "app/[id]/deep/page.tsx"),
        ("app/[id]/file?.{ts,tsx}", "app/[id]/file1.tsx"),
        (
            "app/{group}/part[ver]/**/file-?.{rs,ts}",
            "app/{group}/part[ver]/deep/file-1.rs",
        ),
        ("app/{a,b}/*.tsx", "app/a/page.tsx"),
        // r4: a wildcard before the bracketed or braced directory.
        (
            "packages/*/app/[id]/page.tsx",
            "packages/web/app/[id]/page.tsx",
        ),
        ("app/{shop,blog}/[id]/page.tsx", "app/shop/[id]/page.tsx"),
        ("app/[id]/**/[slug]/*.tsx", "app/[id]/x/[slug]/page.tsx"),
        ("app/*/[id]/*.tsx", "app/shop/[id]/page.tsx"),
        ("app/?/[id]/page.tsx", "app/a/[id]/page.tsx"),
        ("app/[id]*/page.tsx", "app/[id]x/page.tsx"),
        ("packages/*/app/[id]", "packages/web/app/[id]/page.tsx"),
        // r6: an alternation with a `/` still names a directory or a literal route, and
        // the escaped literal names `*` and `?` are directories like `[id]`.
        ("src/{a/b,c}", "src/a/b/page.tsx"),
        ("src/{a/b,c}", "src/c/page.tsx"),
        ("src/{a/b,c}/[id]/page.tsx", "src/a/b/[id]/page.tsx"),
        ("packages/*/app/[*]", "packages/web/app/*/page.tsx"),
        ("packages/*/app/[?]", "packages/web/app/?/page.tsx"),
        // r7: a chain of alternations past any spelling-out cap.
        (
            "src/{a/b,c}/{d,e}/{f,g}/{h,i}/{j,k}/{l,m}",
            "src/a/b/d/f/h/j/l/page.tsx",
        ),
        (
            "src/{a/b,c}/{d,e}/{f,g}/{h,i}/{j,k}/{l,m}/[id]/page.tsx",
            "src/a/b/d/f/h/j/l/[id]/page.tsx",
        ),
    ] {
        let fx = Fx::new();
        let twin = fx.project(&fx.repos.twin);
        assert_eq!(
            fx.acquire("alice", &fx.repos.main, &[pattern])
                .unwrap()
                .granted
                .len(),
            1
        );
        assert_eq!(
            fx.store
                .guard_edit("bob", &twin, &[path.into()], "auto")
                .unwrap()
                .conflicts
                .len(),
            1,
            "manual first: {pattern}"
        );
        fx.store.release_holds("alice", &[]).unwrap();
        assert!(fx
            .store
            .guard_edit("bob", &twin, &[path.into()], "auto")
            .unwrap()
            .conflicts
            .is_empty());
        assert_eq!(
            fx.acquire("alice", &fx.repos.main, &[pattern])
                .unwrap()
                .conflicts
                .len(),
            1,
            "auto first: {pattern}"
        );
    }
    // Must stay free: a different route directory, a different package subtree, and a
    // file the tail does not name.
    let fx = Fx::new();
    let twin = fx.project(&fx.repos.twin);
    for pattern in [
        "packages/*/app/[id]/page.tsx",
        "app/{shop,blog}/[id]/page.tsx",
        "app/[id]/**/[slug]/*.tsx",
        "src/{a/b,c}/[id]/page.tsx",
        "packages/*/app/[*]",
        "src/{a/b,c}/{d,e}/{f,g}/{h,i}/{j,k}/{l,m}/[id]/page.tsx",
    ] {
        assert_eq!(
            fx.acquire("alice", &fx.repos.main, &[pattern])
                .unwrap()
                .granted
                .len(),
            1
        );
    }
    for path in [
        "packages/web/app/[idx]/page.tsx",
        "packages/web/lib/[id]/page.tsx",
        "app/docs/[id]/page.tsx",
        "app/other/[slug]/page.tsx",
        "packages/web/app/[id]/page.css",
        "src/a/[id]/page.tsx",
        "src/a/b/[idx]/page.tsx",
        "packages/web/app/x/page.tsx",
        "src/a/b/d/f/h/j/x/[id]/page.tsx",
        "src/a/b/d/f/h/j/l/[idx]/page.tsx",
    ] {
        assert!(
            fx.store
                .guard_edit("bob", &twin, &[path.into()], "auto")
                .unwrap()
                .conflicts
                .is_empty(),
            "{path} must stay free"
        );
    }
}

/// r6: an agent's own glob hold covers the files it names through an alternation with a
/// `/` or through the escaped literal names `*` and `?`, so the edit guard takes no second
/// hold for them.
#[test]
fn own_glob_holds_cover_brace_and_escaped_wildcard_files() {
    let fx = Fx::new();
    let main = fx.project(&fx.repos.main);
    let granted = fx
        .acquire(
            "alice",
            &fx.repos.main,
            &[
                "src/{a/b,c}",
                "src/{a/b,c}/[id]/page.tsx",
                "packages/*/app/[*]",
                "lib/{a/b,c}/{d,e}/{f,g}/{h,i}/{j,k}/{l,m}/[id]/page.tsx",
            ],
        )
        .unwrap();
    assert_eq!(granted.granted.len(), 4);
    for path in [
        "src/a/b/page.tsx",
        "src/c/page.tsx",
        "src/a/b/[id]/page.tsx",
        "packages/web/app/*/page.tsx",
        "lib/a/b/d/f/h/j/l/[id]/page.tsx",
    ] {
        let guard = fx
            .store
            .guard_edit("alice", &main, &[path.into()], "auto")
            .unwrap();
        assert_eq!(guard.covered, [path], "{path}");
        assert!(guard.held.is_empty(), "{path}");
    }
    // A file outside the alternation gets a hold of its own.
    let guard = fx
        .store
        .guard_edit("alice", &main, &["src/a/x.ts".into()], "auto")
        .unwrap();
    assert!(guard.covered.is_empty());
    assert_eq!(guard.held.len(), 1);
}

/// r5: `*` and `src/*` name the entries at their own depth. They are granted without
/// `--repo-wide` and must not hold everything below the way `**` does.
#[test]
fn wildcard_only_globs_hold_one_level() {
    let fx = Fx::new();
    let twin = fx.project(&fx.repos.twin);
    for pattern in ["*", "src/*"] {
        let granted = fx.acquire("alice", &fx.repos.main, &[pattern]).unwrap();
        assert_eq!(granted.granted.len(), 1, "{pattern}");
    }
    for (path, blocked) in [
        ("top.rs", true),
        ("src/a.rs", true),
        ("src/deep/a.rs", false),
        ("a/b/c.rs", false),
    ] {
        let conflicts = fx
            .store
            .guard_edit("bob", &twin, &[path.into()], "auto")
            .unwrap()
            .conflicts;
        assert_eq!(!conflicts.is_empty(), blocked, "{path}");
    }
}

#[test]
fn manual_bracket_directory_holds_conflict_with_literal_files_in_both_orders() {
    for pattern in ["app/[id]", "app/[id]/**"] {
        let fx = Fx::new();
        let main = fx.project(&fx.repos.main);
        let twin = fx.project(&fx.repos.twin);
        // The directory need not exist for an explicit intent hold to protect it.
        let manual = fx.acquire("alice", &fx.repos.main, &[pattern]).unwrap();
        assert_eq!(manual.granted.len(), 1);
        let edit = fx
            .store
            .guard_edit("bob", &twin, &["app/[id]/page.tsx".into()], "auto")
            .unwrap();
        assert_eq!(edit.conflicts.len(), 1, "manual first: {pattern}");
        fx.store.release_holds("alice", &[]).unwrap();
        assert!(fx
            .store
            .guard_edit("bob", &twin, &["app/[id]/page.tsx".into()], "auto")
            .unwrap()
            .conflicts
            .is_empty());
        let manual = fx.acquire("alice", &fx.repos.main, &[pattern]).unwrap();
        assert!(manual.granted.is_empty(), "auto first: {pattern}");
        assert_eq!(manual.conflicts.len(), 1);
        // A real wildcard still protects ordinary files as well.
        fx.acquire("carol", &fx.repos.main, &["src/**"]).unwrap();
        assert_eq!(
            fx.store
                .guard_edit("alice", &main, &["src/a.rs".into()], "auto")
                .unwrap()
                .conflicts
                .len(),
            1
        );
    }
}

#[test]
fn a_cancelled_release_request_does_not_suppress_a_fresh_ask() {
    let fx = Fx::new();
    let held = fx
        .acquire("alice", &fx.repos.main, &["src/a.rs"])
        .unwrap()
        .granted
        .remove(0);
    let first = fx.store.request_hold_release("bob", &held.hold_id).unwrap();
    assert_eq!(
        fx.store
            .claim_announcement("alice", &[first.message_id], "post_tool", None)
            .unwrap()
            .len(),
        1
    );
    fx.store
        .update_request(RequestUpdate {
            agent: "bob".into(),
            request_id: first.request_id.unwrap(),
            action: RequestAction::Cancel,
            note: None,
            due_in_seconds: None,
        })
        .unwrap();
    let fresh = fx.store.request_hold_release("bob", &held.hold_id).unwrap();
    assert_eq!(
        fx.store
            .claim_announcement("alice", &[fresh.message_id], "post_tool", None)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn linked_release_requests_finish_on_session_end_or_expiry() {
    for session_end in [true, false] {
        let fx = Fx::new();
        let held = fx
            .acquire("alice", &fx.repos.main, &["src/a.rs"])
            .unwrap()
            .granted
            .remove(0);
        assert!(fx
            .store
            .request_hold_release("alice", &held.hold_id)
            .is_err());
        let bob = fx.store.request_hold_release("bob", &held.hold_id).unwrap();
        let carol = fx
            .store
            .request_hold_release("carol", &held.hold_id)
            .unwrap();
        if session_end {
            fx.store.heartbeat("alice", true).unwrap();
            assert_eq!(fx.store.agent("alice").unwrap().status, "offline");
        } else {
            fx.advance_secs(601);
            fx.store.sweep().unwrap();
        }
        fx.store.sweep().unwrap();
        for message in [bob, carol] {
            assert_eq!(
                fx.store
                    .request(message.request_id.as_ref().unwrap())
                    .unwrap()
                    .status,
                RequestStatus::Done
            );
            assert_eq!(
                fx.store
                    .inbox(&message.from, InboxQuery::default())
                    .unwrap()
                    .messages
                    .len(),
                1
            );
        }
        assert!(fx.store.request_hold_release("bob", &held.hold_id).is_err());
        let new_hold = fx
            .acquire("alice", &fx.repos.main, &["src/a.rs"])
            .unwrap()
            .granted
            .remove(0);
        assert_ne!(held.hold_id, new_hold.hold_id);
        let fresh = fx
            .store
            .request_hold_release("bob", &new_hold.hold_id)
            .unwrap();
        assert_eq!(
            fx.store
                .request(fresh.request_id.as_ref().unwrap())
                .unwrap()
                .status,
            RequestStatus::Open
        );
    }
}

/// #137 review P2: the hold id already identifies its owner. A release ask must reach that
/// agent by id even after a second agent with the same name makes the stored display
/// address ambiguous, and it must still be reused and closed when the hold ends.
#[test]
fn release_ask_reaches_the_holder_by_id_after_its_name_becomes_ambiguous() {
    let fx = Fx::new();
    let held = fx
        .acquire("alice", &fx.repos.main, &["src/a.rs"])
        .unwrap()
        .granted
        .remove(0);
    let holder_id = fx.store.agent("alice").unwrap().agent_id;
    let main = fx.project(&fx.repos.main);
    let twin = fx
        .store
        .register_agent(RegisterAgent {
            name: "alice".into(),
            program: Program::Codex,
            project: Some(main),
            model: None,
            description: None,
        })
        .unwrap();
    assert!(
        fx.store.agent("alice").is_err(),
        "the original holder's name is now ambiguous"
    );

    let ask = fx
        .store
        .request_hold_release("bob", &held.hold_id)
        .expect("the hold id identifies its owner");
    let request_id = ask.request_id.clone().unwrap();
    let again = fx.store.request_hold_release("bob", &held.hold_id).unwrap();
    assert_eq!(
        again.message_id, ask.message_id,
        "the outstanding ask is reused"
    );
    // A different agent that merely shares the name is not the holder.
    let from_twin = fx
        .store
        .request_hold_release(&twin.address, &held.hold_id)
        .expect("a same-named agent is not the holder");
    assert_ne!(from_twin.message_id, ask.message_id);
    // Both asks are delivered to the original holder only, never to the twin.
    let db = rusqlite::Connection::open(fx.store.db_path().unwrap()).unwrap();
    for message in [&ask, &from_twin] {
        let recipients: Vec<String> = db
            .prepare("SELECT agent_id FROM deliveries WHERE message_id = ?1")
            .unwrap()
            .query_map([&message.message_id], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(recipients, std::slice::from_ref(&holder_id));
    }
    assert_ne!(holder_id, twin.agent_id);

    fx.advance_secs(601);
    fx.store.sweep().unwrap();
    assert_eq!(
        fx.store.request(&request_id).unwrap().status,
        RequestStatus::Done
    );
    let done = fx.store.inbox("bob", InboxQuery::default()).unwrap();
    assert_eq!(done.messages.len(), 1, "bob gets the Done notice");
}

#[test]
fn push_dedupe_preserves_sender_audience_importance_and_thread_changes() {
    let fx = Fx::new();
    let base = SendMessage {
        from: "alice".into(),
        to: vec!["bob".into()],
        subject: "Same subject".into(),
        body: "same body".into(),
        ..Default::default()
    };
    let first = fx.store.send_message(base.clone()).unwrap();
    let variants = [
        SendMessage {
            from: "carol".into(),
            ..base.clone()
        },
        SendMessage {
            to: vec!["bob".into(), "carol".into()],
            ..base.clone()
        },
        SendMessage {
            importance: Importance::Urgent,
            ..base.clone()
        },
        SendMessage {
            thread_id: Some(first.thread_id.clone()),
            ..base.clone()
        },
    ];
    let mut ids = vec![first.message_id];
    for input in variants {
        ids.push(fx.store.send_message(input).unwrap().message_id);
    }
    assert_eq!(
        fx.store
            .claim_announcement("bob", &ids, "post_tool", None)
            .unwrap()
            .len(),
        5
    );
}

#[test]
fn identical_pushes_are_coalesced_without_losing_mail_or_wake_retries() {
    let fx = Fx::new();
    let send = |body: &str| {
        fx.store
            .send_message(SendMessage {
                from: "alice".into(),
                to: vec!["bob".into()],
                subject: "Holds came back".into(),
                body: body.into(),
                ..Default::default()
            })
            .unwrap()
    };
    let a = send("same");
    let b = send("same");
    let ids = vec![a.message_id.clone(), b.message_id.clone()];
    let claimed = fx
        .store
        .claim_announcement("bob", &ids, "wake", None)
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert!(fx
        .store
        .unannounced("bob", "", 100)
        .unwrap()
        .items
        .is_empty());
    assert_eq!(
        fx.store
            .inbox("bob", InboxQuery::default())
            .unwrap()
            .messages
            .len(),
        2
    );
    // A failed wake gives back both the representative and its suppressed duplicates.
    fx.store.release_announcement("bob", &claimed).unwrap();
    assert_eq!(
        fx.store
            .claim_announcement("bob", &ids, "post_tool", Some("codex"))
            .unwrap()
            .len(),
        1
    );
    let different = send("changed body");
    assert_eq!(
        fx.store
            .claim_announcement("bob", &[different.message_id], "post_tool", None)
            .unwrap()
            .len(),
        1
    );
    fx.advance_secs(301);
    let later = send("same");
    assert_eq!(
        fx.store
            .claim_announcement("bob", &[later.message_id], "post_tool", None)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn blocked_retries_are_deduplicated_and_ping_once_after_release() {
    let fx = Fx::new();
    let main = fx.project(&fx.repos.main);
    let twin = fx.project(&fx.repos.twin);
    fx.acquire("alice", &fx.repos.main, &["src/a.rs"]).unwrap();
    for attempt in 0..20 {
        let blocked = fx
            .store
            .guard_edit("bob", &twin, &["src/a.rs".into()], "auto")
            .unwrap();
        assert_eq!(blocked.conflicts.len(), 1);
        assert_eq!(blocked.first_notice, attempt == 0);
        assert_eq!(blocked.retry_after_seconds, 30);
    }
    for (advance, expected) in [(30, 60), (60, 120), (120, 240), (240, 300)] {
        fx.advance_secs(advance);
        let blocked = fx
            .store
            .guard_edit("bob", &twin, &["src/a.rs".into()], "auto")
            .unwrap();
        assert!(!blocked.first_notice);
        assert_eq!(blocked.retry_after_seconds, expected);
    }
    assert_eq!(
        fx.kinds().iter().filter(|k| *k == "hold.blocked").count(),
        1
    );
    // The subscription survives opening another store connection.
    let store = Store::open_with_clock(&fx.repos.home, Clock::Manual(fx.clock.clone()))
        .unwrap()
        .unguarded_repair();
    store.release_holds("alice", &[]).unwrap();
    store.sweep().unwrap();
    store.sweep().unwrap();
    let inbox = store.inbox("bob", InboxQuery::default()).unwrap();
    assert_eq!(inbox.messages.len(), 1);
    assert_eq!(inbox.messages[0].message.subject, "Hold free: src/a.rs");
    assert!(inbox.messages[0].message.body.contains("not reserved"));
    assert!(store
        .guard_edit("bob", &main, &["src/a.rs".into()], "auto")
        .unwrap()
        .conflicts
        .is_empty());
}

#[test]
fn hold_free_ping_compares_literal_paths_after_restack() {
    let fx = Fx::new();
    let main = fx.project(&fx.repos.main);
    let twin = fx.project(&fx.repos.twin);
    let path = "app/[id]/page.tsx";
    fx.store
        .guard_edit("alice", &main, &[path.into()], "auto")
        .unwrap();
    fx.store
        .guard_edit("carol", &main, &["app/i/page.tsx".into()], "auto")
        .unwrap();
    assert!(!fx
        .store
        .guard_edit("bob", &twin, &[path.into()], "auto")
        .unwrap()
        .conflicts
        .is_empty());
    fx.store.release_holds("alice", &[]).unwrap();
    let inbox = fx.store.inbox("bob", InboxQuery::default()).unwrap();
    assert_eq!(
        inbox.messages.len(),
        1,
        "the ordinary route must not delay a literal-route ping"
    );
    assert_eq!(
        inbox.messages[0].message.subject,
        "Hold free: app/[id]/page.tsx"
    );
}

/// Integration of #108 (r6 manual-glob matching) with #110 (durable waits): the recheck
/// before a `Hold free` ping uses the same per-component matcher as the edit guard, so a
/// manual alternation that names the file's directory keeps the wait open.
#[test]
fn hold_free_ping_waits_for_a_manual_alternation_naming_the_directory() {
    let fx = Fx::new();
    let main = fx.project(&fx.repos.main);
    let twin = fx.project(&fx.repos.twin);
    let path = "src/a/b/page.tsx";
    for (name, pattern) in [("alice", path), ("carol", "src/{a/b,c}")] {
        fx.store
            .acquire_holds(AcquireHolds {
                agent: name.into(),
                project: main.clone(),
                paths: vec![pattern.into()],
                ttl_seconds: Some(600),
                exclusive: false,
                reason: None,
                repo_wide: false,
            })
            .unwrap();
    }
    let blocked = fx
        .store
        .guard_edit("bob", &twin, &[path.into()], "auto")
        .unwrap();
    assert_eq!(blocked.conflicts.len(), 2, "{:?}", blocked.conflicts);
    fx.store.release_holds("alice", &[]).unwrap();
    assert!(
        fx.store
            .inbox("bob", InboxQuery::default())
            .unwrap()
            .messages
            .is_empty(),
        "carol's alternation still holds the file's directory"
    );
    fx.store.release_holds("carol", &[]).unwrap();
    let inbox = fx.store.inbox("bob", InboxQuery::default()).unwrap();
    assert_eq!(inbox.messages.len(), 1);
    assert_eq!(
        inbox.messages[0].message.subject,
        "Hold free: src/a/b/page.tsx"
    );
}

#[test]
fn hold_free_ping_waits_for_every_holder_and_covers_expiry() {
    let fx = Fx::new();
    let main = fx.project(&fx.repos.main);
    for name in ["alice", "carol"] {
        fx.store
            .acquire_holds(AcquireHolds {
                agent: name.into(),
                project: main.clone(),
                paths: vec!["src/a.rs".into()],
                ttl_seconds: Some(60),
                exclusive: false,
                reason: None,
                repo_wide: false,
            })
            .unwrap();
    }
    fx.store
        .guard_edit("bob", &main, &["src/a.rs".into()], "auto")
        .unwrap();
    fx.store.heartbeat("alice", true).unwrap();
    assert!(fx
        .store
        .inbox("bob", InboxQuery::default())
        .unwrap()
        .messages
        .is_empty());
    fx.advance_secs(61);
    fx.store.sweep().unwrap();
    assert_eq!(fx.store.agent("alice").unwrap().status, "offline");
    assert_eq!(
        fx.store
            .inbox("bob", InboxQuery::default())
            .unwrap()
            .messages
            .len(),
        1
    );
}

#[test]
fn ended_one_of_two_blockers_does_not_keep_unrelated_writes_slow() {
    use std::time::Instant;
    let mut timings = Vec::new();
    for n in [400, 2000] {
        let fx = Fx::new();
        let main = fx.project(&fx.repos.main);
        let twin = fx.project(&fx.repos.twin);
        let other = fx.project(&fx.repos.other);
        let time_unrelated = |label: &str| {
            (0..5)
                .map(|i| {
                    let started = Instant::now();
                    fx.store.heartbeat("carol", false).unwrap();
                    let guard = fx
                        .store
                        .guard_edit("carol", &other, &[format!("{label}{i}.rs")], "auto")
                        .unwrap();
                    assert!(guard.conflicts.is_empty());
                    started.elapsed()
                })
                .min()
                .unwrap()
        };
        let baseline = time_unrelated("before");
        for agent in ["alice", "carol"] {
            fx.store
                .acquire_holds(AcquireHolds {
                    agent: agent.into(),
                    project: main.clone(),
                    paths: vec!["src/**".into()],
                    ttl_seconds: Some(3600),
                    exclusive: false,
                    reason: None,
                    repo_wide: false,
                })
                .unwrap();
        }
        for i in 0..n {
            assert_eq!(
                fx.store
                    .guard_edit("bob", &twin, &[format!("src/f{i}.rs")], "auto")
                    .unwrap()
                    .conflicts
                    .len(),
                2
            );
        }
        fx.store.release_holds("alice", &[]).unwrap();
        assert!(fx
            .store
            .inbox("bob", InboxQuery::default())
            .unwrap()
            .messages
            .is_empty());
        let loaded = time_unrelated("after");
        eprintln!("two holders, {n} waits: baseline {baseline:?}, remaining blocker {loaded:?}");
        timings.push((n, baseline, loaded));
        let db = rusqlite::Connection::open(fx.repos.home.join("another.db")).unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM hold_waits", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            n
        );
        fx.store.release_holds("carol", &[]).unwrap();
        let pings = || {
            db.query_row(
                "SELECT count(*) FROM messages WHERE subject LIKE 'Hold free:%'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        };
        assert_eq!(pings(), n);
        fx.store.sweep().unwrap();
        assert_eq!(pings(), n);
    }
    assert!(
        timings
            .iter()
            .all(|(_, baseline, loaded)| *loaded < *baseline * 10),
        "ended blockers keep unrelated writes expensive: {timings:?}"
    );
}

#[test]
fn still_blocked_recheck_tracks_replacement_without_resetting_announcements() {
    let fx = Fx::new();
    let main = fx.project(&fx.repos.main);
    let twin = fx.project(&fx.repos.twin);
    let take = |agent: &str| {
        fx.store
            .acquire_holds(AcquireHolds {
                agent: agent.into(),
                project: main.clone(),
                paths: vec!["src/**".into()],
                ttl_seconds: Some(3600),
                exclusive: false,
                reason: None,
                repo_wide: false,
            })
            .unwrap()
    };
    take("alice");
    take("carol");
    let edit = || {
        fx.store
            .guard_edit("bob", &twin, &["src/a.rs".into()], "auto")
            .unwrap()
    };
    assert!(edit().first_notice);
    let db = rusqlite::Connection::open(fx.repos.home.join("another.db")).unwrap();
    let state = || {
        db.query_row(
            "SELECT blockers, holder_id, retry_at, backoff_seconds FROM hold_waits",
            [],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            },
        )
        .unwrap()
    };
    let original = state();
    let blocked_events = || {
        fx.kinds()
            .iter()
            .filter(|kind| *kind == "hold.blocked")
            .count()
    };
    assert_eq!(blocked_events(), 1);
    fx.store.release_holds("alice", &[]).unwrap();
    fx.store.sweep().unwrap();
    let remaining = state();
    let ids: Vec<String> = serde_json::from_str(&remaining.0).unwrap();
    assert_eq!(ids.len(), 1, "processed ended blockers must be retired");
    assert_eq!(
        (&remaining.1, remaining.2, remaining.3),
        (&original.1, original.2, original.3),
        "bookkeeping must not reset the original sender/backoff"
    );
    assert_eq!(blocked_events(), 1, "bookkeeping is not another denial");
    // A waiter retry still observes the changed conflict set as a first notice once.
    assert!(edit().first_notice);
    assert!(!edit().first_notice);
    assert_eq!(blocked_events(), 2);
    let replacement = take("alice").granted[0].hold_id.clone();
    fx.store.release_holds("carol", &[]).unwrap();
    fx.store.sweep().unwrap();
    let replaced: Vec<String> = serde_json::from_str(&state().0).unwrap();
    assert_eq!(replaced, [replacement]);
    assert!(fx
        .store
        .inbox("bob", InboxQuery::default())
        .unwrap()
        .messages
        .is_empty());
    assert!(
        edit().first_notice,
        "a replacement hold is a newly announced conflict"
    );
    assert!(!edit().first_notice);
    assert_eq!(blocked_events(), 3);
    fx.store.release_holds("alice", &[]).unwrap();
    fx.store.sweep().unwrap();
    assert_eq!(
        fx.store
            .inbox("bob", InboxQuery::default())
            .unwrap()
            .messages
            .len(),
        1
    );
    fx.store.sweep().unwrap();
    assert_eq!(
        fx.store
            .inbox("bob", InboxQuery::default())
            .unwrap()
            .messages
            .len(),
        1
    );
}

#[test]
fn blocked_recheck_update_failure_does_not_fail_release_or_lose_the_wait() {
    let fx = Fx::new();
    let twin = fx.project(&fx.repos.twin);
    let broad = fx
        .acquire("alice", &fx.repos.main, &["src/**"])
        .unwrap()
        .granted[0]
        .hold_id
        .clone();
    let narrow = fx
        .acquire("alice", &fx.repos.main, &["src/a.rs"])
        .unwrap()
        .granted[0]
        .hold_id
        .clone();
    assert_eq!(
        fx.store
            .guard_edit("bob", &twin, &["src/a.rs".into()], "auto")
            .unwrap()
            .conflicts
            .len(),
        2
    );
    let db = rusqlite::Connection::open(fx.repos.home.join(DB_FILE)).unwrap();
    db.execute_batch("CREATE TRIGGER refresh_failure BEFORE UPDATE ON hold_waits BEGIN SELECT RAISE(ABORT, 'injected refresh failure'); END;").unwrap();
    fx.store.release_holds("alice", &[narrow]).unwrap();
    assert!(fx.kinds().iter().any(|kind| kind == "hold.recheck_failed"));
    assert_eq!(
        db.query_row("SELECT count(*) FROM hold_waits", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert!(fx
        .store
        .inbox("bob", InboxQuery::default())
        .unwrap()
        .messages
        .is_empty());
    db.execute_batch("DROP TRIGGER refresh_failure;").unwrap();
    fx.store.sweep().unwrap();
    let blockers: String = db
        .query_row("SELECT blockers FROM hold_waits", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&blockers)
            .unwrap()
            .as_slice(),
        std::slice::from_ref(&broad)
    );
    fx.store.release_holds("alice", &[broad]).unwrap();
    fx.store.sweep().unwrap();
    assert_eq!(
        fx.store
            .inbox("bob", InboxQuery::default())
            .unwrap()
            .messages
            .len(),
        1
    );
}

#[test]
fn edit_guard_refuses_directories_and_keeps_glob_characters_literal() {
    let fx = Fx::new();
    let project = fx.project(&fx.repos.main);
    std::fs::create_dir_all(fx.repos.main.join("src")).unwrap();
    for path in ["src", "src/"] {
        assert!(
            fx.store
                .guard_edit("alice", &project, &[path.into()], "auto")
                .is_err(),
            "{path}"
        );
    }
    assert!(fx.store.holds(HoldFilter::default()).unwrap().is_empty());
    for path in [
        "src/*.rs",
        "src/a?.rs",
        "src/{new}.rs",
        "app/[id]/new.tsx",
        "src/[.rs",
        "src/**",
        "**",
    ] {
        // A new file has a literal name too. It must never become a wildcard hold.
        let guard = fx
            .store
            .guard_edit("alice", &project, &[path.into()], "auto")
            .unwrap();
        assert_eq!(guard.held[0].pattern, globset::escape(path));
        // Windows forbids `*` and `?` in file names, so such a file can only be new there.
        if cfg!(windows) && path.contains(['*', '?']) {
            continue;
        }
        write(&fx.repos.main.join(path), "file");
        let again = fx
            .store
            .guard_edit("alice", &project, &[path.into()], "auto")
            .unwrap();
        assert_eq!(again.covered, [path]);
    }
    assert!(fx
        .store
        .guard_edit("bob", &project, &["src/ordinary.rs".into()], "auto")
        .unwrap()
        .conflicts
        .is_empty());
    fx.store.release_holds("alice", &[]).unwrap();
    fx.store.release_holds("bob", &[]).unwrap();
    // Older auto holds used the unescaped name. Their exact path remains protected.
    fx.acquire("alice", &fx.repos.main, &["app/[id]/new.tsx"])
        .unwrap();
    assert_eq!(
        fx.store
            .guard_edit("bob", &project, &["app/[id]/new.tsx".into()], "auto")
            .unwrap()
            .conflicts
            .len(),
        1
    );
    fx.store.release_holds("alice", &[]).unwrap();
    assert_eq!(
        fx.acquire("alice", &fx.repos.main, &["src"])
            .unwrap()
            .granted
            .len(),
        1
    );
}

#[test]
fn edit_guard_takes_narrow_holds_and_names_the_holder() {
    let fx = Fx::new();
    let r = &fx.repos;
    let main = fx.project(&r.main);
    let twin = fx.project(&r.twin);
    let file = |p: &Path, rel: &str| Repos::path(&p.join(rel));

    let g = fx
        .store
        .guard_edit(
            "alice",
            &main,
            &[file(&r.main, "src/a.rs")],
            "editing (auto)",
        )
        .unwrap();
    assert!(g.conflicts.is_empty());
    assert_eq!(g.held.len(), 1);
    assert_eq!(g.held[0].pattern, "src/a.rs");
    assert!(g.held[0].exclusive);
    assert_eq!(g.held[0].reason.as_deref(), Some("editing (auto)"));
    assert_eq!(g.project, "shop");

    // Editing it again writes nothing.
    let before = fx.kinds().len();
    let again = fx
        .store
        .guard_edit("alice", &main, &["src/a.rs".into()], "editing (auto)")
        .unwrap();
    assert_eq!(again.covered, ["src/a.rs"]);
    assert!(again.held.is_empty());
    assert_eq!(fx.kinds().len(), before, "no events when already covered");

    // Another agent, in a second project of the same checkout, is blocked, and told who
    // holds it.
    let blocked = fx
        .store
        .guard_edit("bob", &twin, &[file(&r.twin, "src/a.rs")], "editing (auto)")
        .unwrap();
    assert!(blocked.held.is_empty());
    assert_eq!(blocked.conflicts.len(), 1);
    assert_eq!(blocked.conflicts[0].held.agent, "alice");
    assert_eq!(blocked.conflicts[0].held.pattern, "src/a.rs");
    assert_eq!(
        blocked.conflicts[0].path, "src/a.rs",
        "reported relative to the project, not as the absolute path the tool sent"
    );
    let ok = fx
        .store
        .guard_edit("bob", &twin, &["src/b.rs".into()], "editing (auto)")
        .unwrap();
    assert_eq!(ok.held.len(), 1);

    // Admission on an existing live file does not renew it.
    fx.advance_secs(70);
    let renewed = fx
        .store
        .guard_edit("alice", &main, &["src/a.rs".into()], "editing (auto)")
        .unwrap();
    assert!(renewed.held.is_empty());
    assert_eq!(renewed.covered, vec!["src/a.rs"]);
    assert!(!fx.kinds().iter().any(|k| k == "hold.renewed"));

    // A shared hold of one's own does not cover an edit; a narrow exclusive one is taken.
    fx.store
        .acquire_holds(AcquireHolds {
            agent: "carol".into(),
            project: main.clone(),
            paths: vec!["docs/**".into()],
            ttl_seconds: None,
            exclusive: false,
            reason: None,
            repo_wide: false,
        })
        .unwrap();
    let g = fx
        .store
        .guard_edit("carol", &main, &["docs/guide.md".into()], "editing (auto)")
        .unwrap();
    assert_eq!(g.held.len(), 1);
    assert_eq!(g.held[0].pattern, "docs/guide.md");

    // The project directory itself is not a file.
    assert!(fx
        .store
        .guard_edit("alice", &main, std::slice::from_ref(&main), "x")
        .is_err());
}

#[test]
fn keepalive_never_renews_file_holds() {
    let fx = Fx::new();
    let main = fx.repos.main.clone();
    fx.acquire("alice", &main, &["src/**"]).unwrap();
    let renewed_events = |fx: &Fx| fx.kinds().iter().filter(|k| *k == "hold.renewed").count();
    let beat = |fx: &Fx| {
        fx.store
            .keepalive("alice", &SessionInfo::default())
            .unwrap()
    };
    assert_eq!(beat(&fx), 0);
    assert_eq!(renewed_events(&fx), 0, "nothing logged when nothing is due");
    fx.advance_secs(301);
    assert_eq!(beat(&fx), 0);
    assert_eq!(renewed_events(&fx), 0);
    let hold = &fx
        .store
        .holds(HoldFilter {
            project: None,
            agent: Some("alice".into()),
        })
        .unwrap()[0];
    assert_eq!(hold.expires_at, fmt_ts(T0 + 600_000));
    // A hold that already lapsed is left for the sweeper, not revived.
    fx.advance_secs(700);
    assert_eq!(beat(&fx), 0);
}

#[test]
fn unannounced_cursor_includes_later_backdated_deliveries() {
    let fx = Fx::new();
    let older = fx
        .store
        .send_message(SendMessage {
            from: "bob".into(),
            to: vec!["carol".into()],
            subject: "delayed delivery".into(),
            body: "body".into(),
            ..Default::default()
        })
        .unwrap();
    let send = || {
        fx.store
            .send_message(SendMessage {
                from: "bob".into(),
                to: vec!["alice".into()],
                subject: "same millisecond".into(),
                body: "body".into(),
                ..Default::default()
            })
            .unwrap()
    };
    let original = [send().message_id, send().message_id];
    let db = rusqlite::Connection::open(fx.store.db_path().unwrap()).unwrap();
    // Make insertion order the opposite of ULID lexical order deterministically.
    let inserted = ["00000000000000000000000002", "00000000000000000000000001"];
    db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    for (old, new) in original.iter().zip(inserted) {
        db.execute(
            "UPDATE messages SET message_id=?1 WHERE message_id=?2",
            rusqlite::params![new, old],
        )
        .unwrap();
        db.execute(
            "UPDATE deliveries SET message_id=?1 WHERE message_id=?2",
            rusqlite::params![new, old],
        )
        .unwrap();
    }
    db.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    let violations: i64 = db
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0);
    let tied = fx.store.unannounced("alice", inserted[0], 10).unwrap();
    assert_eq!(tied.count, 1);
    assert_eq!(tied.items[0].message_id, inserted[1]);
    db.execute(
        "UPDATE messages SET created_at = ?1 WHERE message_id = ?2",
        rusqlite::params![T0, inserted[0]],
    )
    .unwrap();
    db.execute(
        "UPDATE messages SET created_at = ?1 WHERE message_id = ?2",
        rusqlite::params![T0 - 1, inserted[1]],
    )
    .unwrap();

    let all = fx.store.unannounced("alice", "", 10).unwrap();
    assert_eq!(
        all.items
            .iter()
            .map(|item| &item.message_id)
            .collect::<Vec<_>>(),
        vec![&inserted[1], &inserted[0]]
    );
    let after = fx.store.unannounced("alice", inserted[0], 10).unwrap();
    assert_eq!(after.count, 1);
    assert_eq!(after.items[0].message_id, inserted[1]);
    db.execute(
        "INSERT INTO deliveries (message_id, agent_id, node_id, role) SELECT ?1, agent_id, node_id, 'to' FROM agents WHERE name = 'alice'",
        [&older.message_id],
    ).unwrap();
    let after = fx.store.unannounced("alice", inserted[1], 10).unwrap();
    assert_eq!(after.count, 1);
    assert_eq!(after.items[0].message_id, older.message_id);
}

#[test]
fn unannounced_mail_is_claimed_once() {
    let fx = Fx::new();
    let send = |subject: &str, importance: Importance, request: bool| {
        fx.store
            .send_message(SendMessage {
                from: "bob".into(),
                to: vec!["alice".into()],
                subject: subject.into(),
                body: "private body".into(),
                importance,
                request: request.then(NewRequest::default),
                ..Default::default()
            })
            .unwrap()
    };
    let first = send("first", Importance::Normal, false);
    fx.advance_secs(1);
    let second = send("second", Importance::Urgent, true);
    fx.advance_secs(1);
    let third = send("third", Importance::Normal, false);

    let all = fx.store.unannounced("alice", "", 10).unwrap();
    assert_eq!(all.agent, "alice");
    assert_eq!((all.count, all.urgent), (3, 1));
    // Newest delivery first (#132). IDs use real time and random bits, so two created in
    // the same millisecond need not sort in delivery order; the query does not use them.
    let subjects: Vec<_> = all.items.iter().map(|i| i.subject.as_str()).collect();
    assert_eq!(subjects, ["third", "second", "first"]);
    let request = all
        .items
        .iter()
        .find(|i| i.message_id == second.message_id)
        .unwrap();
    assert!(request.request_id.is_some());
    assert_eq!(request.from, "bob");

    let newer = fx.store.unannounced("alice", &first.message_id, 1).unwrap();
    assert_eq!(newer.count, 2);
    assert_eq!(newer.items.len(), 1, "limit caps items, not the count");
    assert_eq!(newer.items[0].message_id, third.message_id);

    // Review F2: a claim is exclusive. The first caller wins; a second (another hook, or
    // the daemon) gets nothing for the same message.
    let third_id = std::slice::from_ref(&third.message_id);
    let won = fx
        .store
        .claim_announcement("alice", third_id, "post_tool", Some("claude"))
        .unwrap();
    assert_eq!(won, third_id);
    assert!(fx
        .store
        .claim_announcement("alice", third_id, "wake", None)
        .unwrap()
        .is_empty());
    assert_eq!(fx.store.unannounced("alice", "", 10).unwrap().count, 2);
    // A released claim (a wake that failed) can be announced again.
    fx.store.release_announcement("alice", third_id).unwrap();
    assert_eq!(fx.store.unannounced("alice", "", 10).unwrap().count, 3);
    // Read mail is no longer a candidate, and cannot be claimed.
    fx.store.acknowledge("alice", third_id, false).unwrap();
    assert_eq!(fx.store.unannounced("alice", "", 10).unwrap().count, 2);
    assert!(fx
        .store
        .claim_announcement("alice", third_id, "prompt", None)
        .unwrap()
        .is_empty());
}

#[test]
fn claims_are_per_agent_even_when_names_sanitize_alike() {
    // Inherited defect from review: `a.b` and `a_b` shared one cursor file, so a notice for
    // one suppressed the other's older mail. Claims are keyed by agent id now.
    let fx = Fx::new();
    for name in ["a.b", "a_b"] {
        fx.store
            .register_agent(RegisterAgent {
                name: name.into(),
                program: Program::Claude,
                project: None,
                model: None,
                description: None,
            })
            .unwrap();
    }
    let send = |to: &str| {
        fx.store
            .send_message(SendMessage {
                from: "bob".into(),
                to: vec![to.into()],
                subject: format!("for {to}"),
                body: "b".into(),
                ..Default::default()
            })
            .unwrap()
    };
    let older = send("a.b");
    fx.advance_secs(1);
    let newer = send("a_b");
    let ids = |m: &Message| vec![m.message_id.clone()];
    assert_eq!(
        fx.store
            .claim_announcement("a_b", &ids(&newer), "post_tool", None)
            .unwrap()
            .len(),
        1
    );
    let left = fx.store.unannounced("a.b", "", 10).unwrap();
    assert_eq!(left.count, 1, "a.b still has its own unannounced mail");
    assert_eq!(left.items[0].message_id, older.message_id);
}

#[test]
fn a_large_burst_is_claimed_in_full() {
    // Review F3: every id of a burst is claimed and recorded, not just the first 50.
    let fx = Fx::new();
    let start = fx.store.now();
    for i in 0..120 {
        fx.store
            .send_message(SendMessage {
                from: "bob".into(),
                to: vec!["alice".into()],
                subject: format!("burst {i}"),
                body: "b".into(),
                ..Default::default()
            })
            .unwrap();
    }
    let unread = fx.store.unannounced("alice", "", 1000).unwrap();
    assert_eq!(unread.items.len(), 120);
    let ids: Vec<String> = unread.items.iter().map(|i| i.message_id.clone()).collect();
    let claimed = fx
        .store
        .claim_announcement("alice", &ids, "post_tool", Some("claude"))
        .unwrap();
    assert_eq!(claimed.len(), 120);
    let s = fx.store.trial_stats(Some(Since::Time(start))).unwrap();
    assert_eq!(s.notices_mid_task, 1);
    assert_eq!(s.messages_pushed, 120);
    assert_eq!(fx.store.unannounced("alice", "", 1000).unwrap().count, 0);
}

#[test]
fn quick_open_never_creates_or_migrates() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let busy = std::time::Duration::from_millis(50);
    assert!(Store::open_quick(&home, busy).is_err());
    assert!(!home.join(DB_FILE).exists(), "nothing created");
    let full = Store::open(&home).unwrap();
    let quick = Store::open_quick(&home, busy).unwrap();
    assert_eq!(quick.node_id(), full.node_id());
    drop((full, quick));
    let db = rusqlite::Connection::open(home.join(DB_FILE)).unwrap();
    db.pragma_update(None, "user_version", 1).unwrap();
    drop(db);
    let err = Store::open_quick(&home, busy).err().unwrap();
    assert!(err.to_string().contains("schema"), "{err}");
}

#[test]
fn stats_count_the_coordination_evidence() {
    let fx = Fx::new();
    let r = &fx.repos;
    let (main, twin) = (fx.project(&r.main), fx.project(&r.twin));
    let edit = |agent: &str, project: &str, path: &str| {
        fx.store
            .guard_edit(agent, project, &[path.to_string()], "editing (auto)")
            .unwrap()
    };
    let start = fx.store.now();

    // Two holds taken by the edit guard, one taken by hand (not counted as automatic).
    edit("alice", &main, "src/a.rs");
    edit("alice", &main, "src/b.rs");
    fx.acquire("carol", &r.main, &["docs/**"]).unwrap();
    // Bob is stopped three times on two files: two notices and two distinct collisions.
    for path in ["src/a.rs", "src/a.rs", "src/b.rs"] {
        assert!(!edit("bob", &twin, path).conflicts.is_empty());
    }

    // Mail: one pushed mid-task and then read, one pushed by a wake and answered, one
    // pushed and ignored, one only shown at a prompt.
    let send = |subject: &str, request: bool| {
        fx.store
            .send_message(SendMessage {
                from: "bob".into(),
                to: vec!["alice".into()],
                subject: subject.into(),
                body: "b".into(),
                request: request.then(NewRequest::default),
                ..Default::default()
            })
            .unwrap()
    };
    let read_me = send("read me", false);
    let answer_me = send("answer me", true);
    let ignore_me = send("ignore me", false);
    let at_prompt = send("at prompt", false);
    fx.advance_secs(1);
    fx.store
        .claim_announcement(
            "alice",
            &[read_me.message_id.clone(), ignore_me.message_id.clone()],
            "post_tool",
            Some("claude"),
        )
        .unwrap();
    let daemon_wake = |outcome: WakeOutcome, detail: &str, message_ids: Vec<String>| {
        fx.store
            .record_transport_wake(&TransportWake {
                agent: "alice".into(),
                session_id: None,
                transport: "herdr".into(),
                target: "w1:p1".into(),
                outcome,
                detail: detail.into(),
                kind: WakeKind::Mail,
                message_ids,
                trigger: "daemon".into(),
            })
            .unwrap();
    };
    daemon_wake(
        WakeOutcome::Started,
        "herdr: agent_prompted (working)",
        vec![answer_me.message_id.clone()],
    );
    daemon_wake(WakeOutcome::Unknown, "herdr: boom", Vec::new());
    fx.store
        .claim_announcement(
            "alice",
            std::slice::from_ref(&at_prompt.message_id),
            "prompt",
            Some("claude"),
        )
        .unwrap();
    fx.advance_secs(1);
    fx.store
        .acknowledge("alice", std::slice::from_ref(&read_me.message_id), false)
        .unwrap();
    fx.store
        .reply(Reply {
            from: "alice".into(),
            message_id: answer_me.message_id.clone(),
            body: "on it".into(),
            all: false,
            importance: None,
            ack_required: false,
        })
        .unwrap();
    let rid = answer_me.request_id.clone().unwrap();
    for action in [RequestAction::Claim, RequestAction::Complete] {
        fx.store
            .update_request(RequestUpdate {
                agent: "alice".into(),
                request_id: rid.clone(),
                action,
                note: None,
                due_in_seconds: None,
            })
            .unwrap();
    }

    let s = fx.store.trial_stats(Some(Since::Time(start))).unwrap();
    assert_eq!(s.edits_blocked, 2);
    assert_eq!(s.collisions, 2);
    assert_eq!(s.holds_auto_acquired, 2);
    assert_eq!(s.notices_mid_task, 1);
    assert_eq!(s.notices_prompt, 1);
    assert_eq!((s.wakes_sent, s.wakes_failed), (1, 1));
    assert_eq!(s.messages_pushed, 3, "two by notice, one by wake");
    assert_eq!(s.pushes_acted_on, 2, "read one, answered one, ignored one");
    assert_eq!(
        (s.requests_created, s.requests_claimed, s.requests_completed),
        (1, 1, 1)
    );
    let b = &s.blocked[0];
    assert_eq!(
        (b.agent.as_str(), b.holder.as_str(), b.path.as_str()),
        ("bob", "alice", "src/b.rs"),
        "newest first, paths relative to the project"
    );
    assert_eq!(
        (b.project.as_str(), b.holder_project.as_str()),
        ("shop-twin", "shop")
    );
    assert_eq!(s.wakes.len(), 2);
    assert_eq!(s.wakes[1].outcome, "started");
    assert_eq!(s.wakes[1].messages, 1);

    // The window works: nothing happened after now.
    fx.advance_secs(10);
    let later = fx
        .store
        .trial_stats(Some(Since::Time(fx.store.now())))
        .unwrap();
    assert_eq!(
        later.edits_blocked + later.notices_mid_task + later.wakes_sent,
        0
    );

    // The new events read well in the activity feed.
    let feed = fx
        .store
        .activity(ActivityQuery {
            since: Some(Since::Time(start)),
            limit: 200,
            kinds: Vec::new(),
        })
        .unwrap();
    let text: Vec<&str> = feed.items.iter().map(|i| i.summary.as_str()).collect();
    assert!(
        text.contains(&"bob was stopped editing shop-twin:src/a.rs (held by alice)"),
        "{text:?}"
    );
    assert!(
        text.contains(&"alice holds shop:src/a.rs (exclusive, taken by the edit hook)"),
        "{text:?}"
    );
    assert!(
        text.contains(&"alice was told about 2 new message(s) mid-task"),
        "{text:?}"
    );
    assert!(text.contains(&"woke alice"), "{text:?}");
    assert!(text.contains(&"could not wake alice"), "{text:?}");
}

#[test]
fn automatic_wake_targets_clear_only_when_they_match() {
    let fx = Fx::new();
    fx.store
        .set_wake_target("alice", &WakeVia::herdr(), "w1:p1", true)
        .unwrap();
    assert!(!fx
        .store
        .clear_auto_wake_target("alice", Some(("herdr", "w9:p9")))
        .unwrap());
    assert!(fx
        .store
        .clear_auto_wake_target("alice", Some(("herdr", "w1:p1")))
        .unwrap());
    fx.store
        .set_wake_target("bob", &WakeVia::herdr(), "reviewer", false)
        .unwrap();
    assert!(!fx.store.clear_auto_wake_target("bob", None).unwrap());
    assert_eq!(
        fx.store.wake_targets().unwrap().len(),
        1,
        "manual target kept"
    );
}

#[test]
fn repo_keys_cached_under_older_rules_are_recomputed() {
    // Review r2 N3: keys cached by an older normalizer (say, the v7 all-lowercase rule)
    // must not survive an upgrade and hide a hold in another project of the checkout.
    // Since D92 the row also carries the checkout, which is what conflicts are found by:
    // a row under older rules is recomputed whatever checkout it names, so the proof runs
    // through a project registered at a subdirectory of the checkout.
    let fx = Fx::new();
    let r = &fx.repos;
    fx.acquire("alice", &r.main, &["src/**"]).unwrap();
    let sub = fx.project(&r.main.join("src"));
    let stale = |key: &str| {
        let db = rusqlite::Connection::open(r.home.join(DB_FILE)).unwrap();
        db.execute(
            "UPDATE project_repos SET repo_key = ?1, checkout = '/stale/elsewhere/' || project_id",
            [key],
        )
        .unwrap();
    };
    let acquire_in_sub = |agent: &str, path: &str| {
        fx.store
            .acquire_holds(AcquireHolds {
                agent: agent.into(),
                project: sub.clone(),
                paths: vec![path.into()],
                ttl_seconds: None,
                exclusive: true,
                reason: None,
                repo_wide: false,
            })
            .unwrap()
    };
    // Every row carries an unversioned key from older rules: both get recomputed.
    stale("example.com/me/shop");
    assert_eq!(acquire_in_sub("bob", "a.rs").conflicts.len(), 1);
    // Rows that disagree (one stale, one current) also end up compared under one rule.
    let db = rusqlite::Connection::open(r.home.join(DB_FILE)).unwrap();
    let main_id: String = db
        .query_row(
            "SELECT project_id FROM projects WHERE slug = 'shop'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    db.execute(
        "UPDATE project_repos SET repo_key = 'example.com/ME/SHOP-old',
           checkout = '/stale/elsewhere' WHERE project_id = ?1",
        [&main_id],
    )
    .unwrap();
    assert_eq!(acquire_in_sub("carol", "b.rs").conflicts.len(), 1);
    // The edit guard sees it too.
    let g = fx
        .store
        .guard_edit("carol", &sub, &["c.rs".into()], "editing (auto)")
        .unwrap();
    assert_eq!(g.conflicts.len(), 1);
    // A row under the current rules with no checkout yet (the column was added to it by
    // the migration) is recomputed as well: the fill never depends on the version alone.
    db.execute(
        "UPDATE project_repos SET checkout = '' WHERE project_id = ?1",
        [&main_id],
    )
    .unwrap();
    assert_eq!(acquire_in_sub("carol", "d.rs").conflicts.len(), 1);
    let filled: String = db
        .query_row(
            "SELECT checkout FROM project_repos WHERE project_id = ?1",
            [&main_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(filled, resolve_existing(&r.main).to_string_lossy());
}

fn register_as(fx: &Fx, name: &str, project: Option<&Path>) -> String {
    fx.store
        .register_agent(RegisterAgent {
            name: name.into(),
            program: Program::Claude,
            project: project.map(Repos::path),
            model: None,
            description: None,
        })
        .unwrap()
        .agent_id
}

/// #110 review P1, result A: the holder's bare name becomes ambiguous after it took the
/// hold. The guard must still deny and record the wait; it must never fail open.
#[test]
fn ambiguous_holder_name_still_denies_the_edit() {
    let fx = Fx::new();
    let main = fx.project(&fx.repos.main);
    fx.project(&fx.repos.twin);
    register_as(&fx, "solo", None);
    fx.acquire("solo", &fx.repos.twin, &["src/d.rs"]).unwrap();
    register_as(&fx, "solo", Some(&fx.repos.twin));
    for attempt in 0..2 {
        let guard = fx
            .store
            .guard_edit("bob", &main, &["src/d.rs".into()], "auto")
            .expect("a real conflict must not fail the guard");
        assert_eq!(guard.conflicts.len(), 1, "attempt {attempt}");
        assert_eq!(guard.first_notice, attempt == 0);
        assert_eq!(guard.retry_after_seconds, 30);
    }
    assert_eq!(
        fx.kinds().iter().filter(|k| *k == "hold.waiting").count(),
        1
    );
}

/// #110 review P1, result B: the waiter's bare name becomes ambiguous after it was blocked.
/// The holder's release, or the sweep after expiry, must still succeed; the ping reaches
/// the waiter by id; the wait is gone; other agents' hold commands keep working.
#[test]
fn ambiguous_waiter_does_not_break_release_sweep_or_other_agents() {
    for expiry in [false, true] {
        let fx = Fx::new();
        let main = fx.project(&fx.repos.main);
        let twin = fx.project(&fx.repos.twin);
        let w = register_as(&fx, "w", None);
        fx.store
            .acquire_holds(AcquireHolds {
                agent: "bob".into(),
                project: twin.clone(),
                paths: vec!["src/e.rs".into()],
                ttl_seconds: Some(60),
                exclusive: true,
                reason: None,
                repo_wide: false,
            })
            .unwrap();
        assert!(!fx
            .store
            .guard_edit("w", &main, &["src/e.rs".into()], "auto")
            .unwrap()
            .conflicts
            .is_empty());
        register_as(&fx, "w", Some(&fx.repos.main));
        if expiry {
            fx.advance_secs(61);
            fx.store
                .sweep()
                .expect("sweep must not depend on resolving the waiter");
        } else {
            fx.store
                .release_holds("bob", &[])
                .expect("release must not depend on resolving the waiter");
        }
        let pings: Vec<_> = fx
            .store
            .events(10_000)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "message.sent" && e.payload["subject"] == "Hold free: src/e.rs")
            .collect();
        assert_eq!(pings.len(), 1, "expiry={expiry}");
        assert_eq!(pings[0].payload["recipients"][0]["agent_id"], w);
        assert_eq!(
            fx.kinds().iter().filter(|k| *k == "hold.available").count(),
            1
        );
        // The wait is gone, so later sweeps and other agents are unaffected.
        fx.store.sweep().unwrap();
        fx.acquire("carol", &fx.repos.main, &["docs/x.md"]).unwrap();
        assert!(fx
            .store
            .check_holds("carol", &main, &["docs/y.md".into()], true)
            .is_ok());
        fx.store.holds(HoldFilter::default()).unwrap();
        fx.store.heartbeat("carol", true).unwrap();
    }
}

/// #111 review P1: the requester's bare name becomes ambiguous before the hold ends. The
/// holder's release must still succeed and the linked request must still close, with its
/// notice sent to the requester by id.
#[test]
fn ambiguous_requester_does_not_break_release() {
    let fx = Fx::new();
    fx.project(&fx.repos.main);
    let w = register_as(&fx, "w", None);
    let held = fx
        .acquire("bob", &fx.repos.twin, &["src/g.rs"])
        .unwrap()
        .granted
        .remove(0);
    let ask = fx.store.request_hold_release("w", &held.hold_id).unwrap();
    register_as(&fx, "w", Some(&fx.repos.main));
    fx.store
        .release_holds("bob", &[])
        .expect("release must not depend on resolving the requester");
    assert_eq!(
        fx.store
            .request(ask.request_id.as_ref().unwrap())
            .unwrap()
            .status,
        RequestStatus::Done
    );
    let done: Vec<_> = fx
        .store
        .events(10_000)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "message.sent" && e.payload["subject"] == "Done: Release src/g.rs?")
        .collect();
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].payload["recipients"][0]["agent_id"], w);
    fx.store.sweep().unwrap();
}

/// #111 review P2: a second `Hold free` ping for the same file within five minutes must
/// still be announced; the waiter was just told to wait for it.
#[test]
fn second_hold_free_ping_for_the_same_file_is_announced() {
    let fx = Fx::new();
    let main = fx.project(&fx.repos.main);
    let twin = fx.project(&fx.repos.twin);
    for round in 0..2 {
        fx.acquire("alice", &fx.repos.main, &["src/b.rs"]).unwrap();
        let blocked = fx
            .store
            .guard_edit("bob", &twin, &["src/b.rs".into()], "auto")
            .unwrap();
        assert!(blocked.first_notice, "round {round}");
        fx.store.release_holds("alice", &[]).unwrap();
        let unread = InboxQuery {
            unread_only: true,
            ..Default::default()
        };
        let inbox = fx.store.inbox("bob", unread).unwrap();
        assert_eq!(inbox.messages.len(), 1, "round {round}");
        assert_eq!(inbox.messages[0].message.subject, "Hold free: src/b.rs");
        let id = inbox.messages[0].message.message_id.clone();
        let claimed = fx
            .store
            .claim_announcement("bob", std::slice::from_ref(&id), "post_tool", None)
            .unwrap();
        assert_eq!(
            claimed,
            vec![id.clone()],
            "round {round}: the ping must be announced"
        );
        fx.store
            .acknowledge("bob", std::slice::from_ref(&id), false)
            .unwrap();
        fx.advance_secs(10);
    }
    assert!(!fx.kinds().iter().any(|k| k == "push.suppressed"));
    assert!(fx
        .store
        .guard_edit("bob", &main, &["src/b.rs".into()], "auto")
        .unwrap()
        .conflicts
        .is_empty());
}

/// Make every wait record fail, or let waits through again.
fn inject_wait_failure(fx: &Fx, on: bool) {
    let db = rusqlite::Connection::open(fx.repos.home.join(DB_FILE)).unwrap();
    db.execute_batch(if on {
        "CREATE TRIGGER hold_waits_inject_failure BEFORE INSERT ON hold_waits
         BEGIN SELECT RAISE(ABORT, 'injected wait-record failure'); END;"
    } else {
        "DROP TRIGGER hold_waits_inject_failure;"
    })
    .unwrap();
}

fn count(fx: &Fx, kind: &str) -> usize {
    fx.kinds().iter().filter(|k| *k == kind).count()
}

/// Wait-record failure must still deny the edit and must say that no wait exists, so the
/// hook never promises a ping that cannot arrive. Retries must not re-send the first hook
/// notice or a second hold.blocked event (D62).
#[test]
fn wait_record_failure_does_not_renotice_on_retry() {
    let fx = Fx::new();
    let main = fx.project(&fx.repos.main);
    let twin = fx.project(&fx.repos.twin);
    fx.acquire("alice", &fx.repos.main, &["src/a.rs"]).unwrap();
    inject_wait_failure(&fx, true);

    let first = fx
        .store
        .guard_edit("bob", &twin, &["src/a.rs".into()], "auto")
        .expect("a wait-record failure must still deny, not fail the guard");
    assert_eq!(first.conflicts.len(), 1);
    assert!(
        first.first_notice,
        "the first failed wait still gets the first notice"
    );
    assert!(
        !first.wait_recorded,
        "a rolled-back wait must not be reported as recorded"
    );
    assert_eq!(first.retry_after_seconds, 30);

    let second = fx
        .store
        .guard_edit("bob", &twin, &["src/a.rs".into()], "auto")
        .expect("retries must still deny");
    assert_eq!(second.conflicts.len(), 1);
    assert!(
        !second.first_notice,
        "a retry after wait-record failure must not re-send the first hook notice"
    );
    assert!(!second.wait_recorded);
    assert_eq!(second.retry_after_seconds, 30);

    assert_eq!(
        count(&fx, "hold.blocked"),
        1,
        "unchanged blockers must not get a second hold.blocked"
    );
    assert!(count(&fx, "hold.wait_failed") >= 2);
    // Nothing was recorded, so a release pings nobody and the next edit goes through.
    fx.store.release_holds("alice", &[]).unwrap();
    fx.store.sweep().unwrap();
    assert!(fx
        .store
        .inbox("bob", InboxQuery::default())
        .unwrap()
        .messages
        .is_empty());
    assert!(fx
        .store
        .guard_edit("bob", &main, &["src/a.rs".into()], "auto")
        .unwrap()
        .conflicts
        .is_empty());
}

/// Dedupe after a wait-record failure is keyed by the exact path and set of blocking
/// hold ids, not by "this agent was blocked in this project before": a new file, a
/// replacement hold and a conflict after an ended wait each get a first notice, while an
/// unchanged blocker stays a short denial however many unrelated events pile up (D62).
#[test]
fn wait_record_failure_notices_new_paths_and_replacement_holds() {
    let fx = Fx::new();
    let main = fx.project(&fx.repos.main);
    let twin = fx.project(&fx.repos.twin);
    let edit = |path: &str| {
        fx.store
            .guard_edit("bob", &twin, &[path.into()], "auto")
            .unwrap()
    };

    // A wait that was recorded and ended by its free-file ping, before failures begin.
    fx.acquire("alice", &fx.repos.main, &["src/c.rs"]).unwrap();
    let recorded = edit("src/c.rs");
    assert!(recorded.first_notice && recorded.wait_recorded);
    fx.store.release_holds("alice", &[]).unwrap();
    assert_eq!(
        fx.store
            .inbox("bob", InboxQuery::default())
            .unwrap()
            .messages
            .len(),
        1
    );
    // A wait that was recorded and ended because the edit went through after expiry.
    fx.acquire("alice", &fx.repos.main, &["src/d.rs"]).unwrap();
    assert!(edit("src/d.rs").wait_recorded);
    fx.advance_secs(601);
    assert!(edit("src/d.rs").conflicts.is_empty());
    assert_eq!(count(&fx, "hold.wait.finished"), 1);
    fx.store.release_holds("bob", &[]).unwrap();

    fx.acquire(
        "alice",
        &fx.repos.main,
        &["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"],
    )
    .unwrap();
    inject_wait_failure(&fx, true);

    // First failure on a file: first notice. Retries: short denial.
    let a = edit("src/a.rs");
    assert!(a.first_notice && !a.wait_recorded);
    for _ in 0..3 {
        let again = edit("src/a.rs");
        assert!(!again.first_notice && !again.wait_recorded);
        assert_eq!(again.retry_after_seconds, 30);
    }
    // Two from the recorded waits on src/c.rs and src/d.rs, one from src/a.rs.
    assert_eq!(count(&fx, "hold.blocked"), 3);

    // Another file held by the same agent is a new conflict.
    assert!(edit("src/b.rs").first_notice, "new path");
    assert!(!edit("src/b.rs").first_notice);
    assert!(
        !edit("src/a.rs").first_notice,
        "interleaved paths stay deduped"
    );
    assert_eq!(count(&fx, "hold.blocked"), 4);

    // Files whose earlier waits ended get a first notice for the new hold.
    assert!(edit("src/c.rs").first_notice, "after a free-file ping");
    assert!(!edit("src/c.rs").first_notice);
    assert!(edit("src/d.rs").first_notice, "after a finished wait");
    assert!(!edit("src/d.rs").first_notice);
    assert_eq!(count(&fx, "hold.blocked"), 6);

    // A replacement hold on the same file is a new blocker set.
    fx.store.release_holds("alice", &[]).unwrap();
    fx.acquire("carol", &fx.repos.main, &["src/a.rs"]).unwrap();
    let replaced = edit("src/a.rs");
    assert!(replaced.first_notice, "replacement hold");
    assert_eq!(replaced.conflicts[0].held.agent, "carol");
    assert!(!edit("src/a.rs").first_notice);
    assert_eq!(count(&fx, "hold.blocked"), 7);

    // Hundreds of unrelated events, including other agents' blocks and wait failures in
    // this project, never push the matching record out of view or stand in for it.
    for i in 0..150 {
        fx.acquire("alice", &fx.repos.main, &[&format!("src/z{i}.rs")])
            .unwrap();
    }
    for _ in 0..3 {
        fx.store
            .guard_edit("alice", &twin, &["src/a.rs".into()], "auto")
            .unwrap();
    }
    assert_eq!(count(&fx, "hold.blocked"), 8, "alice's own first notice");
    assert!(
        !edit("src/a.rs").first_notice,
        "unchanged blockers after noise"
    );
    assert_eq!(count(&fx, "hold.blocked"), 8);
    assert!(
        fx.store
            .events(10_000)
            .unwrap()
            .iter()
            .filter(|e| e.kind == "hold.wait_failed")
            .all(|e| e.payload["path"].is_string() && e.payload["blockers"].is_array()),
        "every failure names its path and blockers"
    );

    // Once waits can be recorded again the next block registers one and the ping comes.
    inject_wait_failure(&fx, false);
    let again = edit("src/a.rs");
    assert!(again.wait_recorded);
    fx.store.release_holds("carol", &[]).unwrap();
    let inbox = fx.store.inbox("bob", InboxQuery::default()).unwrap();
    assert_eq!(inbox.messages.len(), 2);
    assert_eq!(inbox.messages[0].message.subject, "Hold free: src/a.rs");
    assert!(fx
        .store
        .guard_edit("bob", &main, &["src/a.rs".into()], "auto")
        .unwrap()
        .conflicts
        .is_empty());
}

/// A wait recorded for one hold, then a second overlapping hold whose wait update fails:
/// the changed blockers get one first notice, every retry with the same blockers is a
/// short denial, and the wait row's stale signature never stands in for the current
/// blocker set (D62).
#[test]
fn wait_update_failure_does_not_renotice_unchanged_blockers() {
    let fx = Fx::new();
    let twin = fx.project(&fx.repos.twin);
    let edit = |path: &str| {
        fx.store
            .guard_edit("bob", &twin, &[path.into()], "auto")
            .unwrap()
    };
    fx.acquire("alice", &fx.repos.main, &["src/**"]).unwrap();
    let recorded = edit("src/a.rs");
    assert!(recorded.first_notice && recorded.wait_recorded);
    assert_eq!(recorded.conflicts.len(), 1);
    let narrow = fx
        .acquire("alice", &fx.repos.main, &["src/a.rs"])
        .unwrap()
        .granted[0]
        .hold_id
        .clone();
    let db = rusqlite::Connection::open(fx.repos.home.join(DB_FILE)).unwrap();
    db.execute_batch(
        "CREATE TRIGGER hold_waits_inject_update_failure BEFORE UPDATE ON hold_waits
         BEGIN SELECT RAISE(ABORT, 'injected wait-update failure'); END;",
    )
    .unwrap();

    let changed = edit("src/a.rs");
    assert_eq!(changed.conflicts.len(), 2);
    assert!(
        changed.first_notice,
        "a second blocking hold is a changed set"
    );
    assert!(!changed.wait_recorded);
    assert_eq!(changed.retry_after_seconds, 30);
    for _ in 0..3 {
        let again = edit("src/a.rs");
        assert_eq!(again.conflicts.len(), 2);
        assert!(
            !again.first_notice,
            "unchanged blockers after a failed update must be a short denial"
        );
        assert!(!again.wait_recorded);
    }
    assert_eq!(count(&fx, "hold.blocked"), 2);

    // Back to the recorded set: that wait already announced it.
    fx.store.release_holds("alice", &[narrow]).unwrap();
    let back = edit("src/a.rs");
    assert_eq!(back.conflicts.len(), 1);
    assert!(!back.first_notice, "the recorded wait covers this hold");
    assert_eq!(count(&fx, "hold.blocked"), 2);

    // Once updates work again the changed set is recorded and the ping comes once.
    db.execute_batch("DROP TRIGGER hold_waits_inject_update_failure;")
        .unwrap();
    drop(db);
    fx.acquire("alice", &fx.repos.main, &["src/a.rs"]).unwrap();
    let again = edit("src/a.rs");
    assert!(again.first_notice && again.wait_recorded);
    assert_eq!(count(&fx, "hold.blocked"), 3);
    fx.store.release_holds("alice", &[]).unwrap();
    let inbox = fx.store.inbox("bob", InboxQuery::default()).unwrap();
    assert_eq!(inbox.messages.len(), 1);
    assert_eq!(inbox.messages[0].message.subject, "Hold free: src/a.rs");
}
