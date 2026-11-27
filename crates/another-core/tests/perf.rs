//! Performance at realistic size: a home with 50 agents in 5 projects and at least 10,000
//! events (mail, holds taken and released by the edit guard, requests claimed and done),
//! then the median time of each call that runs on a hot path.
//!
//! The ceilings are coarse regression guards, not targets: each is many times the median
//! measured when this test was written (see the table it prints with `--nocapture`), so a
//! debug build on a slow hosted runner passes. They catch order-of-magnitude regressions
//! (an O(n²) loop, a query per row, a sleep or a retry loop on a hot path). They do NOT
//! catch a single missing index at this size: review R1 dropped all twelve non-unique
//! indexes and every ceiling still passed, because scanning a few thousand rows costs
//! microseconds next to a write's commit. Index use is not asserted here.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use another_core::{
    ActivityQuery, AdmitItem, BoardQuery, DoneItem, Importance, InboxQuery, NewRequest,
    PointerKind, Program, ProposeItem, QueueCaller, QueueKind, RegisterAgent, RequestAction,
    RequestFilter, RequestUpdate, SendMessage, Store, DB_FILE,
};

const PROJECTS: usize = 5;
const AGENTS: usize = 50;
const TARGET_EVENTS: i64 = 10_000;
const RUNS: usize = 25;

/// A small deterministic generator, so every run seeds the same home.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 33) as usize) % n
    }
}

struct Home {
    _dir: tempfile::TempDir,
    home: PathBuf,
    store: Store,
    projects: Vec<String>,
    agents: Vec<String>,
}

fn path(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

impl Home {
    fn agent_project(&self, i: usize) -> &str {
        &self.projects[i % PROJECTS]
    }

    fn events(&self) -> i64 {
        self.count("SELECT count(*) FROM events")
    }

    fn count(&self, sql: &str) -> i64 {
        rusqlite::Connection::open(self.home.join(DB_FILE))
            .unwrap()
            .query_row(sql, [], |r| r.get(0))
            .unwrap()
    }

    fn kind(&self, kind: &str) -> i64 {
        self.count(&format!(
            "SELECT count(*) FROM events WHERE kind = '{kind}'"
        ))
    }
}

fn seed() -> Home {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let home = root.join("home");
    let store = Store::open(&home).unwrap();
    let mut projects = Vec::new();
    for p in 0..PROJECTS {
        let dir = root.join(format!("proj{p}"));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(
            dir.join(".git/config"),
            format!("[remote \"origin\"]\n\turl = git@example.com:team/proj{p}.git\n"),
        )
        .unwrap();
        projects.push(path(&dir));
    }
    let mut agents = Vec::new();
    for a in 0..AGENTS {
        let agent = store
            .register_agent(RegisterAgent {
                name: format!("agent{a}"),
                program: Program::Claude,
                project: Some(projects[a % PROJECTS].clone()),
                model: None,
                description: None,
            })
            .unwrap();
        agents.push(agent.address);
    }
    let h = Home {
        _dir: dir,
        home,
        store,
        projects,
        agents,
    };

    let mut rng = Lcg(42);
    let mut round = 0usize;
    while h.events() < TARGET_EVENTS {
        // Mail: 20 messages, some urgent, a few opening requests.
        for i in 0..20 {
            let from = rng.next(AGENTS);
            let to = (from + 1 + rng.next(AGENTS - 1)) % AGENTS;
            let msg = h
                .store
                .send_message(SendMessage {
                    from: h.agents[from].clone(),
                    to: vec![h.agents[to].clone()],
                    subject: format!("round {round} note {i}"),
                    body: "seeded body text".into(),
                    importance: if i % 7 == 0 {
                        Importance::Urgent
                    } else {
                        Importance::Normal
                    },
                    request: (i % 5 == 0).then_some(NewRequest {
                        due_in_seconds: Some(3600),
                    }),
                    ..SendMessage::default()
                })
                .unwrap();
            // Of the requests (every fifth message): a third stay pending, a third are
            // claimed by their recipient, a third are claimed and completed.
            if let Some(request_id) = msg.request_id {
                let actions: &[RequestAction] = match (i / 5) % 3 {
                    0 => &[],
                    1 => &[RequestAction::Claim],
                    _ => &[RequestAction::Claim, RequestAction::Complete],
                };
                for action in actions {
                    h.store
                        .update_request(RequestUpdate {
                            agent: h.agents[to].clone(),
                            request_id: request_id.clone(),
                            action: *action,
                            note: None,
                            due_in_seconds: None,
                        })
                        .unwrap();
                }
            }
        }
        // Edits: the edit guard takes holds; about half are released again.
        for i in 0..20 {
            let a = rng.next(AGENTS);
            let file = format!("src/m{}/f{}.rs", rng.next(40), rng.next(200));
            let guard = h
                .store
                .guard_edit(
                    &h.agents[a],
                    h.agent_project(a),
                    &[file],
                    "editing (taken by the edit hook)",
                )
                .unwrap();
            if i % 2 == 0 {
                let ids: Vec<String> = guard.held.iter().map(|x| x.hold_id.clone()).collect();
                if !ids.is_empty() {
                    h.store.release_holds(&h.agents[a], &ids).unwrap();
                }
            }
        }
        round += 1;
    }
    h
}

/// Median wall time of `RUNS` calls.
fn median(mut f: impl FnMut()) -> Duration {
    let mut times: Vec<Duration> = (0..RUNS)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed()
        })
        .collect();
    times.sort();
    times[RUNS / 2]
}

#[test]
fn hot_paths_stay_fast_at_ten_thousand_events() {
    let started = Instant::now();
    let h = seed();
    let seeded = started.elapsed();
    let events = h.events();
    // The load is what the header says it is (review R1: requests were never claimed).
    let created = h.kind("request.created");
    let claimed = h.kind("request.claimed");
    let completed = h.kind("request.completed");
    assert!(created > 100, "requests created: {created}");
    assert!(claimed * 3 > created, "claimed {claimed} of {created}");
    assert!(
        completed * 5 > created,
        "completed {completed} of {created}"
    );
    let live = h.count("SELECT count(*) FROM holds WHERE released_at IS NULL");
    let released = h.count("SELECT count(*) FROM holds WHERE released_at IS NOT NULL");
    assert!(
        live > 100 && released > 100,
        "holds live {live}, released {released}"
    );
    println!(
        "requests created {created}, claimed {claimed}, completed {completed}; holds live {live}, released {released}"
    );
    let me = &h.agents[3];
    let my_project = h.agent_project(3).to_string();

    let mut file_no = 0usize;
    let cases: Vec<(&str, Duration, Duration)> = vec![
        (
            // pre-edit hook, free file: takes a hold
            "guard_edit (new hold)",
            median(|| {
                file_no += 1;
                h.store
                    .guard_edit(
                        me,
                        &my_project,
                        &[format!("bench/free{file_no}.rs")],
                        "editing (taken by the edit hook)",
                    )
                    .unwrap();
            }),
            Duration::from_millis(250),
        ),
        (
            // pre-edit hook, own file: covered, writes nothing
            "guard_edit (covered)",
            median(|| {
                h.store
                    .guard_edit(
                        me,
                        &my_project,
                        &["bench/free1.rs".into()],
                        "editing (taken by the edit hook)",
                    )
                    .unwrap();
            }),
            Duration::from_millis(250),
        ),
        (
            // post-tool hook on every tool call
            "unannounced",
            median(|| {
                h.store.unannounced(me, "", 1000).unwrap();
            }),
            Duration::from_millis(250),
        ),
        (
            "inbox (unread, 50)",
            median(|| {
                h.store
                    .inbox(
                        me,
                        InboxQuery {
                            unread_only: true,
                            limit: 50,
                            ..InboxQuery::default()
                        },
                    )
                    .unwrap();
            }),
            Duration::from_millis(500),
        ),
        (
            "check_holds",
            median(|| {
                h.store
                    .check_holds(me, &my_project, &["src/m1/f1.rs".into()], true)
                    .unwrap();
            }),
            Duration::from_millis(500),
        ),
        (
            "activity (50)",
            median(|| {
                h.store
                    .activity(ActivityQuery {
                        limit: 50,
                        ..ActivityQuery::default()
                    })
                    .unwrap();
            }),
            Duration::from_secs(1),
        ),
        (
            "activity_summary",
            median(|| {
                h.store.activity_summary(None).unwrap();
            }),
            Duration::from_secs(1),
        ),
        (
            "whats_new (50)",
            median(|| {
                h.store.whats_new(me, None, 50).unwrap();
            }),
            Duration::from_secs(1),
        ),
        (
            "requests (100)",
            median(|| {
                h.store
                    .requests(RequestFilter {
                        limit: 100,
                        ..RequestFilter::default()
                    })
                    .unwrap();
            }),
            Duration::from_millis(500),
        ),
        (
            "board",
            median(|| {
                h.store.board(BoardQuery::default()).unwrap();
            }),
            Duration::from_secs(2),
        ),
        (
            "sweep",
            median(|| {
                h.store.sweep().unwrap();
            }),
            Duration::from_millis(500),
        ),
    ];

    println!("seeded {events} events, {AGENTS} agents, {PROJECTS} projects in {seeded:.1?}");
    println!("{:<24} {:>12} {:>12}", "call", "median", "ceiling");
    for (name, took, ceiling) in &cases {
        println!("{name:<24} {took:>12.2?} {ceiling:>12.0?}");
    }
    assert!(events >= TARGET_EVENTS, "only {events} events seeded");
    for (name, took, ceiling) in &cases {
        assert!(
            took <= ceiling,
            "{name}: median {took:?} over the {ceiling:?} ceiling"
        );
    }
}

/// The core reads `another needs` makes, on the home above with a queue of 1,000 items
/// (200 proposed, 100 ready, 50 claimed, the rest done) and one session row per agent.
/// The command itself (the process, the open, the printing) is measured by hand on a
/// release build; the number is in docs/NEEDS.md.
#[test]
fn needs_cost() {
    const DONE: usize = 650;
    const READY: usize = 100;
    const PROPOSED: usize = 200;
    let started = Instant::now();
    let h = seed();
    let owner = QueueCaller::Owner;
    let propose = |n: usize, kind: QueueKind| {
        h.store
            .queue_propose(
                &owner,
                ProposeItem {
                    title: format!("seeded item {n}"),
                    scope: Some("M7".into()),
                    done_when: Some("the tests pass".into()),
                    kind,
                    ..ProposeItem::default()
                },
            )
            .unwrap()
    };
    let ready = |n: usize| {
        let item = propose(n, QueueKind::Work);
        h.store
            .queue_admit(&owner, &item.label, AdmitItem::default())
            .unwrap();
    };
    for n in 0..DONE {
        ready(n);
        let worker = &h.agents[n % AGENTS];
        let item = h.store.queue_next(worker).unwrap().claimed.unwrap();
        h.store
            .queue_done(
                worker,
                &item.label,
                DoneItem {
                    summary: "done".into(),
                    pointer_kind: PointerKind::Commit,
                    pointer: "abc1234".into(),
                },
            )
            .unwrap();
    }
    // One claim per agent, and one session row per agent that the transport has lost.
    for (n, agent) in h.agents.iter().enumerate() {
        ready(DONE + n);
        h.store.queue_next(agent).unwrap().claimed.unwrap();
        h.store
            .stamp_transport_session(
                agent,
                &format!("session-{n}"),
                "herdr",
                &format!("w1:p{n}"),
                "claude",
                "/work/app",
            )
            .unwrap();
    }
    h.store
        .observe_transport_sessions("herdr", Ok(&[]), None)
        .unwrap();
    // Gone for an hour, so every row needs attention and the command asks, for each
    // agent, whether mail is waiting.
    rusqlite::Connection::open(h.home.join(DB_FILE))
        .unwrap()
        .execute(
            "UPDATE transport_sessions SET state_since = state_since - 3600000,
               attention = 'gone', attention_since = state_since - 3600000",
            [],
        )
        .unwrap();
    for n in 0..READY {
        ready(DONE + AGENTS + n);
    }
    for n in 0..PROPOSED {
        let kind = if n % 10 == 0 {
            QueueKind::Security
        } else {
            QueueKind::Work
        };
        propose(DONE + AGENTS + READY + n, kind);
    }
    let seeded = started.elapsed();
    let items = h.count("SELECT count(*) FROM queue_items");
    let events = h.events();
    assert_eq!(items as usize, DONE + AGENTS + READY + PROPOSED);
    assert_eq!(
        h.count("SELECT count(*) FROM queue_items WHERE state = 'claimed'") as usize,
        AGENTS
    );
    assert_eq!(
        h.count("SELECT count(*) FROM transport_sessions") as usize,
        AGENTS
    );
    assert!(events >= TARGET_EVENTS, "only {events} events seeded");

    let cases: Vec<(&str, Duration, Duration)> = vec![
        (
            "queue_needs_owner",
            median(|| {
                let list = h.store.queue_needs_owner().unwrap();
                assert_eq!(list.decide_now.len() + list.awaiting.len(), PROPOSED);
            }),
            Duration::from_secs(1),
        ),
        (
            "queue_claims",
            median(|| {
                assert_eq!(h.store.queue_claims().unwrap().len(), AGENTS);
            }),
            Duration::from_millis(250),
        ),
        (
            "transport_sessions",
            median(|| {
                assert_eq!(h.store.transport_sessions().unwrap().len(), AGENTS);
            }),
            Duration::from_millis(250),
        ),
        (
            "wake_targets",
            median(|| {
                h.store.wake_targets().unwrap();
            }),
            Duration::from_millis(250),
        ),
        (
            // Everything the command reads, in one go.
            "needs (all reads)",
            median(|| {
                h.store.queue_needs_owner().unwrap();
                h.store.queue_claims().unwrap();
                h.store.transport_sessions().unwrap();
                h.store.wake_targets().unwrap();
                h.store.queue_limits().unwrap();
                h.store.agents().unwrap();
                h.store.node().unwrap();
                for agent in &h.agents {
                    h.store.unannounced(agent, "", 1).unwrap();
                }
            }),
            Duration::from_secs(1),
        ),
    ];
    println!("seeded {items} queue items, {events} events, {AGENTS} agents in {seeded:.1?}");
    println!("{:<24} {:>12} {:>12}", "call", "median", "ceiling");
    for (name, took, ceiling) in &cases {
        println!("{name:<24} {took:>12.2?} {ceiling:>12.0?}");
    }
    for (name, took, ceiling) in &cases {
        assert!(
            took <= ceiling,
            "{name}: median {took:?} over the {ceiling:?} ceiling"
        );
    }
    // For the hand measurement: with ANOTHER_KEEP_NEEDS_HOME set to a directory, the
    // seeded home is copied there once its last connection has closed.
    if let Some(keep) = std::env::var_os("ANOTHER_KEEP_NEEDS_HOME") {
        let Home {
            _dir, home, store, ..
        } = h;
        drop(store);
        let keep = PathBuf::from(keep);
        std::fs::create_dir_all(&keep).unwrap();
        for entry in std::fs::read_dir(&home).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                std::fs::copy(entry.path(), keep.join(entry.file_name())).unwrap();
            }
        }
    }
}
