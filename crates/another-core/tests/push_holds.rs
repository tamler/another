//! Repository-wide holds across worktrees, the edit guard, keepalive, the unread cursor
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

/// A main checkout with an `origin`, a linked worktree of it, and an unrelated repository.
struct Repos {
    _dir: tempfile::TempDir,
    home: PathBuf,
    main: PathBuf,
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
        let other = root.join("blog");
        write(
            &other.join(".git/config"),
            "[remote \"origin\"]\n\turl = https://example.com/me/blog\n",
        );
        Repos {
            home: root.join("home"),
            _dir: dir,
            main,
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
        let store = Store::open_with_clock(&repos.home, clock).unwrap();
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

#[test]
fn sibling_worktrees_and_subprojects_share_holds() {
    let fx = Fx::new();
    let r = &fx.repos;
    let granted = fx.acquire("alice", &r.main, &["src/**"]).unwrap();
    assert_eq!(granted.granted.len(), 1);

    // The worktree is a different project, but the same repository.
    let clash = fx.acquire("bob", &r.wt, &["src/lib.rs"]).unwrap();
    assert!(clash.granted.is_empty());
    assert_eq!(clash.conflicts.len(), 1);
    assert_eq!(clash.conflicts[0].held.agent, "alice");
    assert_eq!(clash.conflicts[0].held.project, "shop");
    assert_eq!(
        fx.acquire("bob", &r.wt, &["docs/x.md"])
            .unwrap()
            .granted
            .len(),
        1
    );

    // A project registered at a subdirectory compares from the repository root.
    let sub = r.main.join("src");
    let clash = fx.acquire("carol", &sub, &["lib.rs"]).unwrap();
    assert_eq!(
        clash.conflicts.len(),
        1,
        "src/lib.rs is under alice's src/**"
    );
    assert!(fx.acquire("carol", &sub, &["../README.md"]).is_err());

    // check_holds sees the sibling too; an unrelated repository does not.
    let conflicts = fx
        .store
        .check_holds("bob", &fx.project(&r.wt), &["src/main.rs".into()], true)
        .unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(
        fx.acquire("bob", &r.other, &["src/lib.rs"])
            .unwrap()
            .granted
            .len(),
        1
    );

    // Listing the worktree's holds includes the sibling's.
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
    assert!(patterns.contains(&("shop", "src/**")), "{patterns:?}");
    assert!(patterns.contains(&("shop-wt", "docs/x.md")), "{patterns:?}");
    assert!(!patterns.iter().any(|(p, _)| *p == "blog"), "{patterns:?}");
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
    // Same path case, different host case and scheme: still one repository.
    let same = fx.repos.root().join("same");
    write(
        &same.join(".git/config"),
        "[remote \"origin\"]\n\turl = https://EXAMPLE.invalid/Team/Shop.git\n",
    );
    assert_eq!(
        fx.acquire("carol", &same, &["src/a.txt"])
            .unwrap()
            .conflicts
            .len(),
        1
    );
}

#[test]
fn projects_from_before_repo_links_are_linked_on_demand() {
    let fx = Fx::new();
    let r = &fx.repos;
    fx.acquire("alice", &r.main, &["src/**"]).unwrap();
    let wt = fx.project(&r.wt);
    // Simulate a database whose projects predate the link cache.
    let db = rusqlite::Connection::open(r.home.join(DB_FILE)).unwrap();
    db.execute("DELETE FROM project_repos", []).unwrap();
    drop(db);
    let report = fx
        .store
        .acquire_holds(AcquireHolds {
            agent: "bob".into(),
            project: wt,
            paths: vec!["src/a.rs".into()],
            ttl_seconds: None,
            exclusive: true,
            reason: None,
            repo_wide: false,
        })
        .unwrap();
    assert_eq!(
        report.conflicts.len(),
        1,
        "legacy hold in the main checkout still counts"
    );
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
    ] {
        let fx = Fx::new();
        let wt = fx.project(&fx.repos.wt);
        assert_eq!(
            fx.acquire("alice", &fx.repos.main, &[pattern])
                .unwrap()
                .granted
                .len(),
            1
        );
        assert_eq!(
            fx.store
                .guard_edit("bob", &wt, &[path.into()], "auto")
                .unwrap()
                .conflicts
                .len(),
            1,
            "manual first: {pattern}"
        );
        fx.store.release_holds("alice", &[]).unwrap();
        assert!(fx
            .store
            .guard_edit("bob", &wt, &[path.into()], "auto")
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
}

#[test]
fn manual_bracket_directory_holds_conflict_with_literal_files_in_both_orders() {
    for pattern in ["app/[id]", "app/[id]/**"] {
        let fx = Fx::new();
        let main = fx.project(&fx.repos.main);
        let wt = fx.project(&fx.repos.wt);
        // The directory need not exist for an explicit intent hold to protect it.
        let manual = fx.acquire("alice", &fx.repos.main, &[pattern]).unwrap();
        assert_eq!(manual.granted.len(), 1);
        let edit = fx
            .store
            .guard_edit("bob", &wt, &["app/[id]/page.tsx".into()], "auto")
            .unwrap();
        assert_eq!(edit.conflicts.len(), 1, "manual first: {pattern}");
        fx.store.release_holds("alice", &[]).unwrap();
        assert!(fx
            .store
            .guard_edit("bob", &wt, &["app/[id]/page.tsx".into()], "auto")
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
    let wt = fx.project(&fx.repos.wt);
    fx.acquire("alice", &fx.repos.main, &["src/a.rs"]).unwrap();
    for attempt in 0..20 {
        let blocked = fx
            .store
            .guard_edit("bob", &wt, &["src/a.rs".into()], "auto")
            .unwrap();
        assert_eq!(blocked.conflicts.len(), 1);
        assert_eq!(blocked.first_notice, attempt == 0);
        assert_eq!(blocked.retry_after_seconds, 30);
    }
    for (advance, expected) in [(30, 60), (60, 120), (120, 240), (240, 300)] {
        fx.advance_secs(advance);
        let blocked = fx
            .store
            .guard_edit("bob", &wt, &["src/a.rs".into()], "auto")
            .unwrap();
        assert!(!blocked.first_notice);
        assert_eq!(blocked.retry_after_seconds, expected);
    }
    assert_eq!(
        fx.kinds().iter().filter(|k| *k == "hold.blocked").count(),
        1
    );
    // The subscription survives opening another store connection.
    let store = Store::open_with_clock(&fx.repos.home, Clock::Manual(fx.clock.clone())).unwrap();
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
    let wt = fx.project(&fx.repos.wt);
    let path = "app/[id]/page.tsx";
    fx.store
        .guard_edit("alice", &main, &[path.into()], "auto")
        .unwrap();
    fx.store
        .guard_edit("carol", &main, &["app/i/page.tsx".into()], "auto")
        .unwrap();
    assert!(!fx
        .store
        .guard_edit("bob", &wt, &[path.into()], "auto")
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
    let wt = fx.project(&r.wt);
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

    // Another agent in a sibling worktree is blocked, and told who holds it.
    let blocked = fx
        .store
        .guard_edit("bob", &wt, &[file(&r.wt, "src/a.rs")], "editing (auto)")
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
        .guard_edit("bob", &wt, &["src/b.rs".into()], "editing (auto)")
        .unwrap();
    assert_eq!(ok.held.len(), 1);

    // Past half its TTL, the covering hold is renewed instead.
    fx.advance_secs(16 * 60);
    let renewed = fx
        .store
        .guard_edit("alice", &main, &["src/a.rs".into()], "editing (auto)")
        .unwrap();
    assert_eq!(renewed.held.len(), 1);
    assert_eq!(renewed.held[0].pattern, "src/a.rs");
    assert!(fx.kinds().iter().any(|k| k == "hold.renewed"));

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
fn keepalive_renews_only_aging_holds() {
    let fx = Fx::new();
    let main = fx.repos.main.clone();
    fx.acquire("alice", &main, &["src/**"]).unwrap();
    let renewed_events = |fx: &Fx| fx.kinds().iter().filter(|k| *k == "hold.renewed").count();
    assert_eq!(fx.store.keepalive("alice").unwrap(), 0);
    assert_eq!(renewed_events(&fx), 0, "nothing logged when nothing is due");
    fx.advance_secs(301);
    assert_eq!(fx.store.keepalive("alice").unwrap(), 1);
    assert_eq!(renewed_events(&fx), 1);
    let hold = &fx
        .store
        .holds(HoldFilter {
            project: None,
            agent: Some("alice".into()),
        })
        .unwrap()[0];
    assert_eq!(hold.expires_at, fmt_ts(T0 + 301_000 + 600_000));
    // A hold that already lapsed is left for the sweeper, not revived.
    fx.advance_secs(700);
    assert_eq!(fx.store.keepalive("alice").unwrap(), 0);
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
    // IDs use real time and random bits, independently of the store's manual clock.
    // The query contract orders by ID, including IDs created in the same millisecond.
    let mut expected = [&first, &second, &third];
    expected.sort_by(|a, b| b.message_id.cmp(&a.message_id));
    assert_eq!(
        all.items.iter().map(|i| &i.message_id).collect::<Vec<_>>(),
        expected.iter().map(|m| &m.message_id).collect::<Vec<_>>()
    );
    let request = all
        .items
        .iter()
        .find(|i| i.message_id == second.message_id)
        .unwrap();
    assert!(request.request_id.is_some());
    assert_eq!(request.from, "bob");

    let newer = fx
        .store
        .unannounced("alice", &expected[2].message_id, 1)
        .unwrap();
    assert_eq!(newer.count, 2);
    assert_eq!(newer.items.len(), 1, "limit caps items, not the count");
    assert_eq!(newer.items[0].message_id, expected[0].message_id);

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
    let (main, wt) = (fx.project(&r.main), fx.project(&r.wt));
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
        assert!(!edit("bob", &wt, path).conflicts.is_empty());
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
    fx.store
        .record_wake(
            "alice",
            WakeVia::Herdr,
            "w1:p1",
            "sent",
            "agent was idle",
            std::slice::from_ref(&answer_me.message_id),
        )
        .unwrap();
    fx.store
        .record_wake(
            "alice",
            WakeVia::Herdr,
            "w1:p1",
            "failed",
            "herdr: boom",
            &[],
        )
        .unwrap();
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
        ("shop-wt", "shop")
    );
    assert_eq!(s.wakes.len(), 2);
    assert_eq!(s.wakes[1].outcome, "sent");
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
        text.contains(&"bob was stopped editing shop-wt:src/a.rs (held by alice)"),
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
    assert!(text.contains(&"woke alice in herdr w1:p1"), "{text:?}");
}

#[test]
fn automatic_wake_targets_clear_only_when_they_match() {
    let fx = Fx::new();
    fx.store
        .set_wake_target("alice", WakeVia::Herdr, "w1:p1", true)
        .unwrap();
    assert!(!fx
        .store
        .clear_auto_wake_target("alice", Some((WakeVia::Herdr, "w9:p9")))
        .unwrap());
    assert!(fx
        .store
        .clear_auto_wake_target("alice", Some((WakeVia::Herdr, "w1:p1")))
        .unwrap());
    fx.store
        .set_wake_target("bob", WakeVia::Herdr, "reviewer", false)
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
    // must not survive an upgrade and hide a sibling's hold.
    let fx = Fx::new();
    let r = &fx.repos;
    fx.acquire("alice", &r.main, &["src/**"]).unwrap();
    let wt = fx.project(&r.wt);
    let stale = |key: &str| {
        let db = rusqlite::Connection::open(r.home.join(DB_FILE)).unwrap();
        db.execute("UPDATE project_repos SET repo_key = ?1", [key])
            .unwrap();
    };
    let acquire_in_wt = |agent: &str, path: &str| {
        fx.store
            .acquire_holds(AcquireHolds {
                agent: agent.into(),
                project: wt.clone(),
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
    assert_eq!(acquire_in_wt("bob", "src/a.rs").conflicts.len(), 1);
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
        "UPDATE project_repos SET repo_key = 'example.com/ME/SHOP-old' WHERE project_id = ?1",
        [&main_id],
    )
    .unwrap();
    drop(db);
    assert_eq!(acquire_in_wt("carol", "src/b.rs").conflicts.len(), 1);
    // The edit guard sees it too.
    let g = fx
        .store
        .guard_edit("carol", &wt, &["src/c.rs".into()], "editing (auto)")
        .unwrap();
    assert_eq!(g.conflicts.len(), 1);
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
    fx.project(&fx.repos.wt);
    register_as(&fx, "solo", None);
    fx.acquire("solo", &fx.repos.wt, &["src/d.rs"]).unwrap();
    register_as(&fx, "solo", Some(&fx.repos.wt));
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
        let wt = fx.project(&fx.repos.wt);
        let w = register_as(&fx, "w", None);
        fx.store
            .acquire_holds(AcquireHolds {
                agent: "bob".into(),
                project: wt.clone(),
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
        .acquire("bob", &fx.repos.wt, &["src/g.rs"])
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
    let wt = fx.project(&fx.repos.wt);
    for round in 0..2 {
        fx.acquire("alice", &fx.repos.main, &["src/b.rs"]).unwrap();
        let blocked = fx
            .store
            .guard_edit("bob", &wt, &["src/b.rs".into()], "auto")
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
/// notice or a second hold.blocked event (D59).
#[test]
fn wait_record_failure_does_not_renotice_on_retry() {
    let fx = Fx::new();
    let main = fx.project(&fx.repos.main);
    let wt = fx.project(&fx.repos.wt);
    fx.acquire("alice", &fx.repos.main, &["src/a.rs"]).unwrap();
    inject_wait_failure(&fx, true);

    let first = fx
        .store
        .guard_edit("bob", &wt, &["src/a.rs".into()], "auto")
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
        .guard_edit("bob", &wt, &["src/a.rs".into()], "auto")
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
/// unchanged blocker stays a short denial however many unrelated events pile up (D59).
#[test]
fn wait_record_failure_notices_new_paths_and_replacement_holds() {
    let fx = Fx::new();
    let main = fx.project(&fx.repos.main);
    let wt = fx.project(&fx.repos.wt);
    let edit = |path: &str| {
        fx.store
            .guard_edit("bob", &wt, &[path.into()], "auto")
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
            .guard_edit("alice", &wt, &["src/a.rs".into()], "auto")
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
