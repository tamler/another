//! Concurrency stress: 50 agents, each on its own connection to one database (as separate
//! processes would be), released together by a barrier, race for the same hold, the same
//! file through the edit guard, and the same request. Each race must have exactly one
//! winner, every loser a clean conflict, and no call a database error (a busy timeout
//! would show up here). Several rounds per race catch an interleaving that only sometimes
//! goes wrong.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};

use another_core::*;

const AGENTS: usize = 50;
const ROUNDS: usize = 5;

fn agent(i: usize) -> String {
    format!("agent{i:02}")
}

/// A home with `lead` and 50 agents registered, and a project checkout.
fn setup() -> (tempfile::TempDir, PathBuf, String) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let project = dir.path().join("shop");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    let store = Store::open(&home).unwrap();
    let project = store
        .ensure_project("t", &project.to_string_lossy())
        .unwrap()
        .abs_path;
    for name in std::iter::once("lead".to_string()).chain((0..AGENTS).map(agent)) {
        store
            .register_agent(RegisterAgent {
                name,
                program: Program::Other,
                project: None,
                model: None,
                description: None,
            })
            .unwrap();
    }
    (dir, home, project)
}

/// Run `f(i, store)` for every agent at once, each on its own connection, and collect the
/// results in agent order.
fn race<T: Send + 'static>(
    home: &Path,
    f: impl Fn(usize, &Store) -> T + Send + Sync + 'static,
) -> Vec<T> {
    let barrier = Arc::new(Barrier::new(AGENTS));
    let f = Arc::new(f);
    let handles: Vec<_> = (0..AGENTS)
        .map(|i| {
            let (barrier, f, home) = (barrier.clone(), f.clone(), home.to_path_buf());
            std::thread::spawn(move || {
                let store = Store::open(&home).unwrap();
                barrier.wait();
                f(i, &store)
            })
        })
        .collect();
    handles.into_iter().map(|h| h.join().unwrap()).collect()
}

#[test]
fn fifty_agents_racing_for_one_exclusive_hold_get_exactly_one() {
    let (_dir, home, project) = setup();
    for round in 0..ROUNDS {
        let path = format!("src/hold{round}.rs");
        let (p, file) = (project.clone(), path.clone());
        let results = race(&home, move |i, s| {
            s.acquire_holds(AcquireHolds {
                agent: agent(i),
                project: p.clone(),
                paths: vec![file.clone()],
                ttl_seconds: Some(600),
                exclusive: true,
                reason: None,
                repo_wide: false,
            })
        });
        let mut winners = 0;
        for (i, r) in results.iter().enumerate() {
            let report = r
                .as_ref()
                .unwrap_or_else(|e| panic!("round {round}, {}: {e}", agent(i)));
            match (report.granted.is_empty(), report.conflicts.is_empty()) {
                (false, true) => winners += 1,
                (true, false) => {}
                other => panic!("round {round}, {}: granted/conflicts {other:?}", agent(i)),
            }
        }
        assert_eq!(winners, 1, "round {round}");
        let store = Store::open(&home).unwrap();
        let live: Vec<_> = store
            .holds(HoldFilter::default())
            .unwrap()
            .into_iter()
            .filter(|h| h.pattern == path)
            .collect();
        assert_eq!(live.len(), 1, "round {round}: {live:?}");
    }
}

#[test]
fn fifty_agents_editing_one_file_through_the_guard_get_exactly_one_hold() {
    let (_dir, home, project) = setup();
    for round in 0..ROUNDS {
        let path = format!("src/edit{round}.rs");
        let (p, file) = (project.clone(), path.clone());
        let results = race(&home, move |i, s| {
            s.guard_edit(&agent(i), &p, std::slice::from_ref(&file), "auto")
        });
        let mut holders = 0;
        for (i, r) in results.iter().enumerate() {
            let guard = r
                .as_ref()
                .unwrap_or_else(|e| panic!("round {round}, {}: {e}", agent(i)));
            match (guard.held.is_empty(), guard.conflicts.is_empty()) {
                (false, true) => holders += 1,
                (true, false) => {}
                other => panic!("round {round}, {}: held/conflicts {other:?}", agent(i)),
            }
        }
        assert_eq!(holders, 1, "round {round}");
        let store = Store::open(&home).unwrap();
        let live = store
            .holds(HoldFilter::default())
            .unwrap()
            .into_iter()
            .filter(|h| h.pattern == path)
            .count();
        assert_eq!(live, 1, "round {round}");
    }
}

#[test]
fn fifty_agents_claiming_one_request_leave_exactly_one_owner() {
    let (_dir, home, _project) = setup();
    for round in 0..ROUNDS {
        let store = Store::open(&home).unwrap();
        let sent = store
            .send_message(SendMessage {
                from: "lead".into(),
                to: (0..AGENTS).map(agent).collect(),
                subject: format!("Race {round}"),
                body: "first one takes it".into(),
                request: Some(NewRequest::default()),
                ..Default::default()
            })
            .unwrap();
        let rid = sent.request_id.clone().expect("a request");
        let id = rid.clone();
        let results = race(&home, move |i, s| {
            s.update_request(RequestUpdate {
                agent: agent(i),
                request_id: id.clone(),
                action: RequestAction::Claim,
                note: None,
                due_in_seconds: None,
            })
        });
        let mut owners = Vec::new();
        for (i, r) in results.iter().enumerate() {
            match r {
                Ok(req) => {
                    assert_eq!(req.status, RequestStatus::Claimed);
                    owners.push(agent(i));
                }
                Err(Error::Conflict(_)) => {}
                Err(e) => panic!("round {round}, {}: {e}", agent(i)),
            }
        }
        assert_eq!(owners.len(), 1, "round {round}: {owners:?}");
        let stored = store.request(&rid).unwrap();
        assert_eq!(stored.status, RequestStatus::Claimed);
        assert_eq!(stored.owner.as_deref(), Some(owners[0].as_str()));
        let claims = store
            .events(100_000)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "request.claimed" && e.payload["request_id"] == rid.as_str())
            .count();
        assert_eq!(claims, 1, "round {round}: one claim event");
    }
}
