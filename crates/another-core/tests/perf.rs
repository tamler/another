//! Performance at realistic size: a home with 50 agents in 5 projects and at least 10,000
//! events (mail, holds taken and released by the edit guard, requests claimed and done),
//! then the median time of each call that runs on a hot path.
//!
//! The ceilings are regression guards, not targets: each is many times the median measured
//! when this test was written (see the table it prints with `--nocapture`), so a debug
//! build on a slow CI runner passes, and an accidental full scan or an N+1 query does not.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use another_core::{
    ActivityQuery, BoardQuery, Importance, InboxQuery, NewRequest, Program, RegisterAgent,
    RequestAction, RequestFilter, RequestUpdate, SendMessage, Store, DB_FILE,
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
        rusqlite::Connection::open(self.home.join(DB_FILE))
            .unwrap()
            .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
            .unwrap()
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
            h.store
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
            if i % 10 == 0 {
                if let Some(req) = h
                    .store
                    .requests(RequestFilter {
                        owner: Some(h.agents[to].clone()),
                        limit: 1,
                        ..RequestFilter::default()
                    })
                    .unwrap()
                    .first()
                {
                    // The newest request of that owner may already be done; its update
                    // is then refused, which is fine for seeding.
                    for action in [RequestAction::Claim, RequestAction::Complete] {
                        let _ = h.store.update_request(RequestUpdate {
                            agent: h.agents[to].clone(),
                            request_id: req.request_id.clone(),
                            action,
                            note: None,
                            due_in_seconds: None,
                        });
                    }
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

    println!(
        "seeded {events} events, {AGENTS} agents, {PROJECTS} projects in {:.1?}",
        seeded
    );
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
