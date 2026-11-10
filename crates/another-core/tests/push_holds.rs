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
    send("second", Importance::Urgent, true);
    fx.advance_secs(1);
    let third = send("third", Importance::Normal, false);

    let all = fx.store.unannounced("alice", "", 10).unwrap();
    assert_eq!(all.agent, "alice");
    assert_eq!((all.count, all.urgent), (3, 1));
    let subjects: Vec<_> = all.items.iter().map(|i| i.subject.as_str()).collect();
    assert_eq!(subjects, ["third", "second", "first"]);
    assert!(all.items[1].request_id.is_some());
    assert_eq!(all.items[1].from, "bob");

    let newer = fx.store.unannounced("alice", &first.message_id, 1).unwrap();
    assert_eq!(newer.count, 2);
    assert_eq!(newer.items.len(), 1, "limit caps items, not the count");

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
    // Bob is stopped three times on two files: three stops, two distinct collisions.
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
    assert_eq!(s.edits_blocked, 3);
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
