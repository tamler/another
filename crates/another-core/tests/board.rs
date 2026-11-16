//! The work board: holds, edits the edit hook claimed or stopped, and what is waiting on
//! whom, per project. Reading it must write nothing.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use another_core::*;

const T0: i64 = 1_800_000_000_000;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn path(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// `shop` with a linked worktree `shop-wt`, and an unrelated `blog`.
struct Fx {
    _dir: tempfile::TempDir,
    main: PathBuf,
    wt: PathBuf,
    blog: PathBuf,
    store: Store,
    clock: Arc<AtomicI64>,
}

impl Fx {
    fn new() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let main = root.join("shop");
        write(
            &main.join(".git/config"),
            "[remote \"origin\"]\n\turl = git@example.com:me/shop.git\n",
        );
        let wt = root.join("shop-wt");
        let wt_git = main.join(".git/worktrees/shop-wt");
        write(&wt.join(".git"), &format!("gitdir: {}\n", wt_git.display()));
        write(&wt_git.join("commondir"), "../..\n");
        let blog = root.join("blog");
        write(
            &blog.join(".git/config"),
            "[remote \"origin\"]\n\turl = https://example.com/me/blog\n",
        );
        let (clock, handle) = Clock::manual(T0);
        let store = Store::open_with_clock(&root.join("home"), clock).unwrap();
        for (name, program, project) in [
            ("alice", Program::Claude, &main),
            ("bob", Program::Codex, &wt),
            ("carol", Program::Grok, &blog),
        ] {
            store
                .register_agent(RegisterAgent {
                    name: name.into(),
                    program,
                    project: Some(path(project)),
                    model: None,
                    description: None,
                })
                .unwrap();
        }
        Fx {
            _dir: dir,
            main,
            wt,
            blog,
            store,
            clock: handle,
        }
    }

    fn advance_secs(&self, secs: i64) {
        self.clock.fetch_add(secs * 1000, Ordering::SeqCst);
    }

    /// What the edit hook does before an edit.
    fn edit(&self, agent: &str, dir: &Path, file: &str) -> EditGuard {
        self.store
            .guard_edit(
                agent,
                &path(dir),
                &[path(&dir.join(file))],
                EDIT_HOOK_HOLD_REASON,
            )
            .unwrap()
    }

    fn board(&self, project: Option<&Path>) -> Board {
        self.store
            .board(BoardQuery {
                project: project.map(path),
                ..Default::default()
            })
            .unwrap()
    }

    fn send(&self, from: &str, to: &str, subject: &str, request: Option<NewRequest>) {
        self.store
            .send_message(SendMessage {
                from: from.into(),
                to: vec![to.into()],
                cc: vec![],
                subject: subject.into(),
                body: "body text".into(),
                importance: Importance::Urgent,
                ack_required: true,
                reply_to: None,
                thread_id: None,
                request,
            })
            .unwrap();
    }
}

fn project<'a>(b: &'a Board, slug: &str) -> &'a BoardProject {
    b.projects
        .iter()
        .find(|p| p.project == slug)
        .unwrap_or_else(|| panic!("no {slug} in {:?}", b.projects))
}

#[test]
fn board_shows_holds_edits_stopped_edits_and_handoffs() {
    let fx = Fx::new();
    assert!(fx
        .edit("alice@shop", &fx.main, "src/a.rs")
        .conflicts
        .is_empty());
    // bob works in a sibling checkout of the same repository: the edit hook stops him.
    let stopped = fx.edit("bob@shop-wt", &fx.wt, "src/a.rs");
    assert_eq!(stopped.conflicts.len(), 1);
    fx.send(
        "alice@shop",
        "carol@blog",
        "Review the parser",
        Some(NewRequest::default()),
    );

    let b = fx.board(None);
    let shop = project(&b, "shop");
    assert_eq!(shop.holds.len(), 1);
    let hold = &shop.holds[0];
    assert_eq!(hold.agent, "alice@shop");
    assert_eq!(hold.pattern, "src/a.rs");
    assert!(hold.exclusive && hold.edit_hook);
    assert_eq!(hold.expires_in_seconds, DEFAULT_HOLD_TTL_SECONDS);
    assert_eq!(shop.edits.len(), 1);
    assert_eq!(shop.edits[0].agent, "alice@shop");
    assert_eq!(shop.edits[0].path, "src/a.rs");

    let wt = project(&b, "shop-wt");
    assert!(wt.holds.is_empty() && wt.edits.is_empty());
    assert_eq!(wt.blocked.len(), 1);
    let s = &wt.blocked[0];
    assert_eq!(
        (s.agent.as_str(), s.path.as_str(), s.holder.as_deref()),
        ("bob@shop-wt", "src/a.rs", Some("alice@shop"))
    );
    assert_eq!(
        (s.holder_project.as_deref(), s.pattern.as_deref()),
        (Some("shop"), Some("src/a.rs"))
    );
    assert!(!b.confined && !b.history_truncated);

    // Nothing happened in blog; it has no section.
    assert!(b.projects.iter().all(|p| p.project != "blog"));

    let carol = b.agents.iter().find(|a| a.address == "carol@blog").unwrap();
    assert_eq!(
        (
            carol.unread,
            carol.urgent_unread,
            carol.awaiting_ack,
            carol.requests
        ),
        (1, 1, 1, 1)
    );
    let alice = b.agents.iter().find(|a| a.address == "alice@shop").unwrap();
    assert_eq!((alice.holds, alice.unread), (1, 0));
    // bob has nothing waiting and holds nothing, but he is listed because his edit is
    // stopped by a hold that is still live (D70).
    let bob = b
        .agents
        .iter()
        .find(|a| a.address == "bob@shop-wt")
        .expect("a blocked agent is listed");
    assert_eq!(bob.state, AgentState::Blocked);
    assert_eq!(
        (bob.unread, bob.awaiting_ack, bob.requests, bob.holds),
        (0, 0, 0, 0)
    );

    assert_eq!(b.requests.len(), 1);
    let r = &b.requests[0];
    assert_eq!(r.title, "Review the parser");
    assert_eq!(
        (r.from.as_deref(), r.to.clone(), r.to_hidden, r.owner_hidden),
        (Some("alice@shop"), vec!["carol@blog".to_string()], 0, false)
    );
    assert_eq!((r.status.as_str(), r.overdue), ("open", false));

    // The board carries no message bodies.
    let text = serde_json::to_string(&b).unwrap();
    assert!(!text.contains("body text"));
}

#[test]
fn reading_the_board_writes_nothing() {
    let fx = Fx::new();
    fx.edit("alice@shop", &fx.main, "src/a.rs");
    fx.send(
        "alice@shop",
        "carol@blog",
        "Due soon",
        Some(NewRequest {
            due_in_seconds: Some(60),
        }),
    );
    // Past the hold's TTL and the request's due time, with no sweep in between.
    fx.advance_secs(DEFAULT_HOLD_TTL_SECONDS + 1);
    let before = fx.store.events(100_000).unwrap().len();
    let seq = fx.store.dashboard_seq().unwrap();

    let b = fx.board(None);
    assert!(
        b.projects.iter().all(|p| p.holds.is_empty()),
        "a lapsed hold is not live"
    );
    // The edit is still history inside the window.
    assert_eq!(project(&b, "shop").edits.len(), 1);
    let r = &b.requests[0];
    assert_eq!((r.status.as_str(), r.overdue), ("open", true));

    assert_eq!(fx.store.events(100_000).unwrap().len(), before);
    assert_eq!(fx.store.dashboard_seq().unwrap(), seq);
    // The board did not sweep: the hold and the request are still there to expire.
    let swept = fx.store.sweep().unwrap();
    assert_eq!((swept.expired_holds, swept.stale_requests), (1, 1));
    let after = fx.board(None);
    assert_eq!(after.requests[0].status, "stale");
    assert!(!after.requests[0].overdue);
}

#[test]
fn project_filter_includes_sibling_checkouts_only() {
    let fx = Fx::new();
    fx.edit("alice@shop", &fx.main, "src/a.rs");
    fx.edit("bob@shop-wt", &fx.wt, "src/b.rs");
    fx.edit("carol@blog", &fx.blog, "post.md");
    fx.send("carol@blog", "bob@shop-wt", "Blog to shop", None);
    fx.send(
        "carol@blog",
        "carol@blog",
        "Blog only",
        Some(NewRequest::default()),
    );

    let b = fx.board(Some(&fx.main));
    assert_eq!(b.project.as_deref(), Some(path(&fx.main).as_str()));
    let mut slugs: Vec<&str> = b.projects.iter().map(|p| p.project.as_str()).collect();
    slugs.sort_unstable();
    assert_eq!(slugs, ["shop", "shop-wt"]);
    let mut agents: Vec<&str> = b.agents.iter().map(|a| a.address.as_str()).collect();
    agents.sort_unstable();
    assert_eq!(agents, ["alice@shop", "bob@shop-wt"]);
    assert!(b.requests.is_empty(), "blog's own request stays out");

    let exact = fx
        .store
        .board(BoardQuery {
            project: Some("shop".into()),
            confined: true,
            ..Default::default()
        })
        .unwrap();
    let slugs: Vec<&str> = exact.projects.iter().map(|p| p.project.as_str()).collect();
    assert_eq!(slugs, ["shop"]);

    // A quiet project named by the filter is still listed.
    let fx2 = Fx::new();
    let quiet = fx2.board(Some(&fx2.blog));
    assert_eq!(quiet.projects.len(), 1);
    assert!(quiet.projects[0].holds.is_empty());

    let missing = fx.store.board(BoardQuery {
        project: Some("nope".into()),
        ..Default::default()
    });
    assert!(matches!(missing, Err(Error::NotFound(_))));
}

#[test]
fn window_and_limit_bound_the_edit_lists() {
    let fx = Fx::new();
    fx.edit("alice@shop", &fx.main, "src/old.rs");
    fx.advance_secs(2 * 3600);
    fx.edit("alice@shop", &fx.main, "src/a.rs");
    fx.edit("alice@shop", &fx.main, "src/b.rs");

    let recent = fx
        .store
        .board(BoardQuery {
            since_ms: Some(fx.store.now() - 3600 * 1000),
            ..Default::default()
        })
        .unwrap();
    let shop = project(&recent, "shop");
    let paths: Vec<&str> = shop.edits.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        paths,
        ["src/b.rs", "src/a.rs"],
        "newest first, old one outside the window"
    );
    assert!(!shop.truncated);

    let one = fx
        .store
        .board(BoardQuery {
            limit: 1,
            ..Default::default()
        })
        .unwrap();
    let shop = project(&one, "shop");
    assert_eq!(shop.edits.len(), 1);
    assert!(shop.truncated);
    // Holds are not cut by the limit (the src/old.rs hold lapsed two hours ago).
    assert_eq!(shop.holds.len(), 2);
}

#[test]
fn claimed_request_waits_on_its_owner() {
    let fx = Fx::new();
    fx.send(
        "alice@shop",
        "carol@blog",
        "Do it",
        Some(NewRequest::default()),
    );
    let id = fx.board(None).requests[0].request_id.clone();
    fx.store
        .update_request(RequestUpdate {
            agent: "bob@shop-wt".into(),
            request_id: id,
            action: RequestAction::Claim,
            note: None,
            due_in_seconds: None,
        })
        .unwrap();
    let b = fx.board(None);
    let r = &b.requests[0];
    assert_eq!(
        (r.status.as_str(), r.owner.as_deref()),
        ("claimed", Some("bob@shop-wt"))
    );
    let bob = b
        .agents
        .iter()
        .find(|a| a.address == "bob@shop-wt")
        .unwrap();
    assert_eq!(bob.requests, 1);
    let carol = b.agents.iter().find(|a| a.address == "carol@blog").unwrap();
    assert_eq!(
        carol.requests, 0,
        "claimed work no longer waits on the recipients"
    );
}

#[test]
fn dashboard_snapshot_lists_recent_and_stopped_edits() {
    let fx = Fx::new();
    fx.edit("alice@shop", &fx.main, "src/a.rs");
    fx.edit("bob@shop-wt", &fx.wt, "src/a.rs");
    let snap = fx
        .store
        .dashboard_snapshot(DashboardView::REDACTED)
        .unwrap();
    assert_eq!(snap.edits.len(), 1);
    assert_eq!(
        (
            snap.edits[0].project.as_str(),
            snap.edits[0].edit.path.as_str()
        ),
        ("shop", "src/a.rs")
    );
    assert_eq!(snap.blocked.len(), 1);
    assert_eq!(
        snap.blocked[0].blocked.holder.as_deref(),
        Some("alice@shop")
    );
    let v = serde_json::to_value(&snap).unwrap();
    assert_eq!(v["edits"][0]["agent"], "alice@shop");
    assert_eq!(v["blocked"][0]["project"], "shop-wt");
}

/// Review round 1, P1 x2: a confined board (what a remote caller gets) must not name agents,
/// projects or hold patterns that belong only to other projects, even inside records that
/// are in view: the holder of a stopped edit, and request senders, recipients and owners.
#[test]
fn a_confined_board_hides_details_from_other_projects() {
    let fx = Fx::new();
    // alice (shop) holds src/a.rs; bob (shop-wt, the sibling checkout) is stopped on it.
    fx.edit("alice@shop", &fx.main, "src/a.rs");
    assert_eq!(
        fx.edit("bob@shop-wt", &fx.wt, "src/a.rs").conflicts.len(),
        1
    );
    // A request from shop to shop-wt and blog, and one from blog to shop-wt that carol owns.
    fx.send(
        "alice@shop",
        "bob@shop-wt",
        "Mixed recipients",
        Some(NewRequest::default()),
    );
    fx.store
        .send_message(SendMessage {
            from: "alice@shop".into(),
            to: vec!["bob@shop-wt".into(), "carol@blog".into()],
            cc: vec![],
            subject: "Two projects".into(),
            body: "x".into(),
            importance: Importance::Normal,
            ack_required: false,
            reply_to: None,
            thread_id: None,
            request: Some(NewRequest::default()),
        })
        .unwrap();
    fx.send(
        "carol@blog",
        "bob@shop-wt",
        "From outside",
        Some(NewRequest::default()),
    );
    let outside = fx
        .board(None)
        .requests
        .iter()
        .find(|r| r.title == "From outside")
        .unwrap()
        .request_id
        .clone();
    fx.store
        .update_request(RequestUpdate {
            agent: "carol@blog".into(),
            request_id: outside,
            action: RequestAction::Claim,
            note: None,
            due_in_seconds: None,
        })
        .unwrap();

    // The local, sibling-aware view keeps every name.
    let local = fx.board(Some(&fx.wt));
    let blocked = &project(&local, "shop-wt").blocked[0];
    assert_eq!(blocked.holder.as_deref(), Some("alice@shop"));

    let b = fx
        .store
        .board(BoardQuery {
            project: Some("shop-wt".into()),
            confined: true,
            ..Default::default()
        })
        .unwrap();
    assert!(b.confined);
    let slugs: Vec<&str> = b.projects.iter().map(|p| p.project.as_str()).collect();
    assert_eq!(slugs, ["shop-wt"]);
    let s = &b.projects[0].blocked[0];
    assert_eq!(
        (s.agent.as_str(), s.path.as_str()),
        ("bob@shop-wt", "src/a.rs")
    );
    assert_eq!(
        (&s.holder, &s.holder_project, &s.pattern),
        (&None, &None, &None)
    );

    let by_title = |t: &str| b.requests.iter().find(|r| r.title == t).unwrap();
    let mixed = by_title("Mixed recipients");
    assert_eq!(
        mixed.from, None,
        "alice is registered to shop, outside the view"
    );
    assert_eq!(mixed.to, ["bob@shop-wt"]);
    let two = by_title("Two projects");
    assert_eq!(
        (two.to.clone(), two.to_hidden),
        (vec!["bob@shop-wt".to_string()], 1)
    );
    let claimed = by_title("From outside");
    assert_eq!(
        (claimed.from.as_deref(), claimed.owner.as_deref()),
        (None, None)
    );
    assert!(claimed.owner_hidden);
    let agents: Vec<&str> = b.agents.iter().map(|a| a.address.as_str()).collect();
    assert_eq!(agents, ["bob@shop-wt"]);

    let text = serde_json::to_string(&b).unwrap();
    for leaked in ["alice@shop", "carol@blog", "\"shop\"", "blog"] {
        assert!(!text.contains(leaked), "{leaked} leaked: {text}");
    }

    let unnamed = fx.store.board(BoardQuery {
        confined: true,
        ..Default::default()
    });
    assert!(matches!(unnamed, Err(Error::Invalid(_))));
}

/// Review round 1, P2: the event scan cap counts only events the board can show, and
/// reaching it is reported instead of silently dropping in-window edits.
#[test]
fn the_event_scan_cap_counts_relevant_events_and_reports_when_hit() {
    let fx = Fx::new();
    fx.edit("alice@shop", &fx.main, "src/first.rs");
    let db = rusqlite::Connection::open(fx._dir.path().join("home").join(DB_FILE)).unwrap();
    // Copies of that event, in the same window: `n` of them, with `auto` set as given and
    // in the given project.
    let copies = |n: u32, auto: bool, project_id: Option<&str>, tag: &str| {
        db.execute(
            "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < ?1)
             INSERT INTO events (event_id, node_id, ts, actor, kind, payload)
             SELECT ?4 || x, e.node_id, e.ts, e.actor, 'hold.updated',
                    json_set(e.payload, '$.auto', json(?2), '$.pattern', 'src/' || ?4 || x,
                             '$.project_id', coalesce(?3, json_extract(e.payload, '$.project_id')))
             FROM n CROSS JOIN (SELECT * FROM events WHERE kind = 'hold.acquired' LIMIT 1) e",
            rusqlite::params![n, if auto { "true" } else { "false" }, project_id, tag],
        )
        .unwrap();
    };
    let blog_id = fx
        .store
        .projects()
        .unwrap()
        .into_iter()
        .find(|p| p.slug == "blog")
        .unwrap()
        .project_id;
    // Manual hold updates, and edit-hook events in another project, do not count.
    copies(BOARD_EVENT_SCAN_CAP, false, None, "manual-");
    copies(BOARD_EVENT_SCAN_CAP, true, Some(&blog_id), "blog-");
    let shop = || {
        fx.store
            .board(BoardQuery {
                project: Some("shop".into()),
                confined: true,
                ..Default::default()
            })
            .unwrap()
    };
    let b = shop();
    assert_eq!(
        b.projects[0].edits.len(),
        1,
        "manual updates must not use up the cap"
    );
    assert!(!b.history_truncated);

    // More edit-hook events in view than the cap: reported.
    copies(BOARD_EVENT_SCAN_CAP, true, None, "auto-");
    let b = shop();
    assert!(b.history_truncated);
    assert!(
        b.projects[0].truncated,
        "the edit list is also cut at its limit"
    );
    let all = fx.board(None);
    assert!(all.history_truncated);
}

fn state(fx: &Fx, agent: &str) -> AgentStatus {
    fx.store.agent_status(agent).unwrap()
}

/// D70: agents report Herdr's states. Recent activity is `working`, a quiet session is
/// `idle`, an edit stopped by a hold that is still live is `blocked` with a message, and an
/// ended session is `unknown`. The board, the dashboard and `agent_status` agree.
#[test]
fn agent_state_follows_herdr_idle_working_and_blocked() {
    let fx = Fx::new();
    assert_eq!(state(&fx, "alice@shop").state, AgentState::Working);
    assert!(fx
        .edit("alice@shop", &fx.main, "src/a.rs")
        .conflicts
        .is_empty());
    assert_eq!(
        fx.edit("bob@shop-wt", &fx.wt, "src/a.rs").conflicts.len(),
        1
    );

    let bob = state(&fx, "bob@shop-wt");
    assert_eq!(bob.state, AgentState::Blocked);
    let message = bob.message.clone().expect("a blocked agent says why");
    assert!(
        message.contains("src/a.rs") && message.contains("alice@shop"),
        "{message}"
    );
    assert_eq!(bob.blocked_by.len(), 1);
    assert_eq!(state(&fx, "alice@shop").message, None);

    // The board lists a blocked agent even when nothing else is waiting on it.
    let board = fx.board(None);
    let row = board
        .agents
        .iter()
        .find(|a| a.address == "bob@shop-wt")
        .expect("a blocked agent is on the board");
    assert_eq!(row.state, AgentState::Blocked);
    assert_eq!(row.blocked_message.as_deref(), Some(message.as_str()));
    let dash = fx.store.dashboard_snapshot(DashboardView::LOCAL).unwrap();
    let row = dash
        .agents
        .iter()
        .find(|a| a.address == "bob@shop-wt")
        .unwrap();
    assert_eq!(row.state, AgentState::Blocked);
    assert_eq!(row.blocked_message.as_deref(), Some(message.as_str()));
    assert_eq!(
        serde_json::to_value(row.state).unwrap(),
        serde_json::json!("blocked"),
        "states serialize with Herdr's spelling"
    );

    // When the hold ends, bob is no longer blocked.
    fx.store.release_holds("alice@shop", &[]).unwrap();
    assert_eq!(state(&fx, "bob@shop-wt").state, AgentState::Working);

    // Quiet past the live window: idle. An ended session: unknown.
    fx.advance_secs(11 * 60);
    assert_eq!(state(&fx, "alice@shop").state, AgentState::Idle);
    fx.store.heartbeat("carol@blog", true).unwrap();
    let carol = state(&fx, "carol@blog");
    assert_eq!(carol.state, AgentState::Unknown);
    assert_eq!(carol.message, None);
}

/// A stop is the agent's current state only until it edits something else, the hold ends,
/// or the stop falls out of the live window.
#[test]
fn a_later_edit_or_an_old_stop_does_not_leave_an_agent_blocked() {
    let fx = Fx::new();
    assert!(fx
        .edit("alice@shop", &fx.main, "src/a.rs")
        .conflicts
        .is_empty());
    assert_eq!(
        fx.edit("bob@shop-wt", &fx.wt, "src/a.rs").conflicts.len(),
        1
    );
    let first = state(&fx, "bob@shop-wt");
    assert_eq!(first.state, AgentState::Blocked);

    fx.advance_secs(1);
    assert!(fx
        .edit("bob@shop-wt", &fx.wt, "src/b.rs")
        .conflicts
        .is_empty());
    assert_eq!(
        state(&fx, "bob@shop-wt").state,
        AgentState::Working,
        "bob moved on to another file"
    );

    // Back to the held file inside the 30-second backoff: the guard still denies it, but
    // with unchanged blockers it writes no event (D62), so the board cannot see the retry.
    fx.advance_secs(1);
    assert_eq!(
        fx.edit("bob@shop-wt", &fx.wt, "src/a.rs").conflicts.len(),
        1
    );
    assert_eq!(state(&fx, "bob@shop-wt").state, AgentState::Working);

    // A retry after the window ends starts a new one (`hold.waiting`): blocked again, with
    // the same message as the first stop.
    fx.advance_secs(30);
    assert_eq!(
        fx.edit("bob@shop-wt", &fx.wt, "src/a.rs").conflicts.len(),
        1
    );
    let again = state(&fx, "bob@shop-wt");
    assert_eq!(again.state, AgentState::Blocked);
    assert_eq!(again.message, first.message);
    assert_eq!(again.blocked_by, first.blocked_by);

    // Alice's hold (30 minutes) is still live, but the stop is older than the live window.
    fx.advance_secs(11 * 60);
    let bob = state(&fx, "bob@shop-wt");
    assert_eq!(bob.state, AgentState::Idle);
    assert_eq!(bob.message, None);
}

#[test]
fn reading_agent_state_writes_nothing() {
    let fx = Fx::new();
    assert!(fx
        .edit("alice@shop", &fx.main, "src/a.rs")
        .conflicts
        .is_empty());
    fx.edit("bob@shop-wt", &fx.wt, "src/a.rs");
    let before = fx.store.events(100_000).unwrap().len();
    for agent in ["alice@shop", "bob@shop-wt", "carol@blog"] {
        state(&fx, agent);
    }
    assert_eq!(fx.store.events(100_000).unwrap().len(), before);
}
