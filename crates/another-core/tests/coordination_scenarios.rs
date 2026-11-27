//! Opt-in multi-agent scenario evaluations for Q43 (scope M10).
//!
//! These scenarios use temporary Another homes and scripted actors. They do not
//! call model APIs. Run explicitly with:
//! `cargo test -p another-core --test coordination_scenarios -- --ignored`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use another_core::*;

const START_MS: i64 = 1_800_000_000_000;
const GRACE_MS: i64 = 86_400_000;
const WAIT_LIMIT: Duration = Duration::from_secs(12);

fn tempdir() -> tempfile::TempDir {
    if let Some(root) = std::env::var_os("ANOTHER_EVAL_ROOT") {
        tempfile::Builder::new()
            .prefix("another-eval-")
            .tempdir_in(root)
            .expect("create evaluation home under ANOTHER_EVAL_ROOT")
    } else {
        tempfile::Builder::new()
            .prefix("another-eval-")
            .tempdir()
            .expect("create evaluation home")
    }
}

fn register(store: &Store, name: &str, project: Option<&str>) {
    store
        .register_agent(RegisterAgent {
            name: name.to_owned(),
            program: Program::Claude,
            project: project.map(str::to_owned),
            model: None,
            description: None,
        })
        .expect("register scripted agent");
}

fn propose(store: &Store, who: &str, title: &str) -> QueueItem {
    store
        .queue_propose(
            &QueueCaller::agent(who),
            ProposeItem {
                title: title.to_owned(),
                scope: Some("M10".to_owned()),
                done_when: Some("scenario invariant holds".to_owned()),
                ..Default::default()
            },
        )
        .expect("propose scripted item")
}

fn admit(store: &Store, item: &QueueItem) {
    store
        .queue_admit(&QueueCaller::Owner, &item.label, AdmitItem::default())
        .expect("admit scripted item");
}

fn session(store: &Store, agent: &str, phase: SessionPhase, id: &str) {
    store
        .session(
            agent,
            phase,
            SessionInfo {
                client: Some("claude".to_owned()),
                session_id: Some(id.to_owned()),
                ..Default::default()
            },
        )
        .expect("record scripted session step");
}

fn wait_file(path: &Path, deadline: Instant) -> bool {
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    path.exists()
}

fn make_child(home: &Path, rendezvous: &Path, actor: &str) -> std::io::Result<Child> {
    Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "queue_next_race_child",
            "--ignored",
            "--nocapture",
        ])
        .env("ANOTHER_EVAL_CHILD", "1")
        .env("ANOTHER_EVAL_HOME", home)
        .env("ANOTHER_EVAL_RENDEZVOUS", rendezvous)
        .env("ANOTHER_EVAL_ACTOR", actor)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

fn finish_children(children: Vec<Child>) -> Vec<std::process::Output> {
    children
        .into_iter()
        .map(|child| child.wait_with_output().expect("reap scenario child"))
        .collect()
}

/// A separate process waits for the parent's bounded rendezvous and races queue_next.
#[test]
#[ignore = "child process for the opt-in queue_next race"]
fn queue_next_race_child() {
    if std::env::var("ANOTHER_EVAL_CHILD").as_deref() != Ok("1") {
        return;
    }
    let home = PathBuf::from(std::env::var_os("ANOTHER_EVAL_HOME").expect("child home"));
    let rendezvous =
        PathBuf::from(std::env::var_os("ANOTHER_EVAL_RENDEZVOUS").expect("child rendezvous"));
    let actor = std::env::var("ANOTHER_EVAL_ACTOR").expect("child actor");
    fs::write(rendezvous.join(format!("ready-{actor}")), "ready").expect("mark child ready");
    assert!(
        wait_file(&rendezvous.join("go"), Instant::now() + WAIT_LIMIT),
        "timed out waiting for race gate"
    );
    let result = Store::open(&home)
        .expect("open shared evaluation home")
        .queue_next(&actor)
        .expect("scripted queue_next");
    fs::write(
        rendezvous.join(format!("result-{actor}")),
        result
            .claimed
            .map(|item| item.label)
            .unwrap_or_else(|| "empty".to_owned()),
    )
    .expect("write child outcome");
}

#[test]
#[ignore = "opt-in multi-agent coordination evaluation"]
fn two_agents_racing_next_get_exactly_one_claim() {
    let dir = tempdir();
    let home = dir.path().join("home");
    let setup = Store::open(&home).unwrap();
    register(&setup, "racer-a", None);
    register(&setup, "racer-b", None);
    let item = propose(&setup, "racer-a", "one contested item");
    admit(&setup, &item);

    let rendezvous = dir.path().join("rendezvous");
    fs::create_dir(&rendezvous).unwrap();
    let mut children = Vec::new();
    for actor in ["racer-a", "racer-b"] {
        match make_child(&home, &rendezvous, actor) {
            Ok(child) => children.push(child),
            Err(error) => {
                fs::write(rendezvous.join("go"), "go").unwrap();
                let _outputs = finish_children(children);
                panic!("launch queue race child: {error}");
            }
        }
    }

    let deadline = Instant::now() + WAIT_LIMIT;
    let ready = wait_file(&rendezvous.join("ready-racer-a"), deadline)
        && wait_file(&rendezvous.join("ready-racer-b"), deadline);
    fs::write(rendezvous.join("go"), "go").unwrap();
    let outputs = finish_children(children);
    assert!(ready, "both children should reach the bounded start gate");
    assert!(
        outputs.iter().all(|out| out.status.success()),
        "child exits: {:?}",
        outputs
            .iter()
            .map(|out| out.status.code())
            .collect::<Vec<_>>()
    );
    let a = fs::read_to_string(rendezvous.join("result-racer-a")).unwrap();
    let b = fs::read_to_string(rendezvous.join("result-racer-b")).unwrap();
    assert_eq!(usize::from(a != "empty") + usize::from(b != "empty"), 1);
    let claimed = if a == "empty" { b } else { a };
    assert_eq!(claimed, item.label);
    let audit = setup.queue_audit(None).unwrap();
    assert!(audit.ok, "{:?}", audit.findings);
    assert_eq!(audit.claims, 1);
}

#[test]
#[ignore = "opt-in multi-agent coordination evaluation"]
fn overlapping_exclusive_holds_have_exactly_one_winner() {
    let dir = tempdir();
    let home = dir.path().join("home");
    let project_path = dir.path().join("checkout");
    fs::create_dir(&project_path).unwrap();
    let project_path = project_path.to_string_lossy().into_owned();
    let store = Store::open(&home).unwrap();
    register(&store, "holder-a", Some(&project_path));
    register(&store, "holder-b", Some(&project_path));
    let project = store.ensure_project("holder-a", &project_path).unwrap();

    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let mut joins = Vec::new();
    let mut gates = Vec::new();
    for actor in ["holder-a", "holder-b"] {
        let home = home.clone();
        let slug = project.slug.clone();
        let ready_tx = ready_tx.clone();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        gates.push(go_tx);
        joins.push(thread::spawn(move || {
            let store = Store::open(&home).unwrap();
            ready_tx.send(()).unwrap();
            go_rx
                .recv_timeout(WAIT_LIMIT)
                .expect("bounded hold race gate");
            store
                .acquire_holds(AcquireHolds {
                    agent: actor.to_owned(),
                    project: slug,
                    paths: vec!["src/shared.rs".to_owned()],
                    ttl_seconds: None,
                    exclusive: true,
                    reason: None,
                    repo_wide: false,
                })
                .unwrap()
        }));
    }
    drop(ready_tx);
    let ready =
        ready_rx.recv_timeout(WAIT_LIMIT).is_ok() && ready_rx.recv_timeout(WAIT_LIMIT).is_ok();
    for gate in gates {
        let _ = gate.send(());
    }
    let reports: Vec<HoldReport> = joins
        .into_iter()
        .map(|join| join.join().expect("hold actor completed"))
        .collect();
    assert!(ready, "both hold actors reached bounded start gate");
    assert_eq!(reports.iter().filter(|r| r.granted.len() == 1).count(), 1);
    assert_eq!(
        reports.iter().filter(|r| !r.conflicts.is_empty()).count(),
        1
    );
    let holds = store
        .holds(HoldFilter {
            project: Some(project.slug),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(holds.len(), 1);
    assert!(holds[0].exclusive);
}

#[test]
#[ignore = "opt-in multi-agent coordination evaluation"]
fn claim_release_requires_owning_session_end_or_silent_grace() {
    let dir = tempdir();
    let home = dir.path().join("home");
    let (clock, clock_handle) = Clock::manual(START_MS);
    let store = Store::open_with_clock(&home, clock).unwrap();
    register(&store, "claimant", None);
    register(&store, "observer", None);
    let session_item = propose(&store, "claimant", "session-bound claim");
    admit(&store, &session_item);

    session(&store, "claimant", SessionPhase::Start, "owner-session");
    assert_eq!(
        store.queue_next("claimant").unwrap().claimed.unwrap().label,
        session_item.label
    );
    store.heartbeat("claimant", true).unwrap();
    session(&store, "claimant", SessionPhase::Start, "other-session");
    session(&store, "claimant", SessionPhase::End, "other-session");
    assert_eq!(
        store.queue_item(&session_item.label).unwrap().state,
        QueueState::Claimed
    );
    session(&store, "claimant", SessionPhase::End, "owner-session");
    assert_eq!(
        store.queue_item(&session_item.label).unwrap().state,
        QueueState::Ready
    );

    let grace_claim = store.queue_next("claimant").unwrap().claimed.unwrap();
    assert_eq!(grace_claim.label, session_item.label);
    let session_bound = store
        .queue_item(&grace_claim.item_id)
        .unwrap()
        .claim
        .unwrap()
        .session_bound;
    assert!(
        !session_bound,
        "ended session cannot bind the new claim"
    );
    clock_handle.fetch_add(GRACE_MS - 1, Ordering::SeqCst);
    assert!(store.queue_next("observer").unwrap().claimed.is_none());
    assert_eq!(
        store.queue_item(&grace_claim.item_id).unwrap().state,
        QueueState::Claimed
    );
    assert_eq!(
        store
            .queue_item(&grace_claim.item_id)
            .unwrap()
            .claim
            .unwrap()
            .agent,
        "claimant"
    );
    clock_handle.fetch_add(1, Ordering::SeqCst);
    let released_by_grace = store.queue_next("observer").unwrap();
    assert_eq!(
        released_by_grace.claimed.unwrap().item_id,
        grace_claim.item_id
    );
    assert_eq!(
        store
            .queue_item(&grace_claim.item_id)
            .unwrap()
            .claim
            .unwrap()
            .agent,
        "observer"
    );
    let reasons: Vec<String> = store
        .events(1000)
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "queue.released")
        .filter_map(|event| event.payload["reason"].as_str().map(str::to_owned))
        .collect();
    assert!(reasons.iter().any(|reason| reason == "session_end"));
    assert!(reasons.iter().any(|reason| reason == "grace"));
}

#[test]
#[ignore = "opt-in multi-agent coordination evaluation"]
fn admission_racing_next_never_claims_before_admission() {
    // A single interleaving can miss the admit-then-claim ordering. Loop and
    // always assert the post-join state: either admitted-then-claimed, or
    // admitted and still Ready if next ran first.
    for attempt in 0..32 {
        let dir = tempdir();
        let home = dir.path().join("home");
        let setup = Store::open(&home).unwrap();
        register(&setup, "admitter", None);
        register(&setup, "worker", None);
        let item = propose(&setup, "admitter", "admission race");
        let store = Store::open(&home).unwrap();
        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        let (admission_go_tx, admission_go_rx) = mpsc::channel::<()>();
        let admission_item = item.clone();
        let admission_store = Store::open(&home).unwrap();
        let admission_ready = ready_tx.clone();
        let admission = thread::spawn(move || {
            admission_ready.send(()).unwrap();
            admission_go_rx
                .recv_timeout(WAIT_LIMIT)
                .expect("bounded admission gate");
            admission_store
                .queue_admit(
                    &QueueCaller::Owner,
                    &admission_item.label,
                    AdmitItem::default(),
                )
                .unwrap();
        });
        let next_store = Store::open(&home).unwrap();
        let next_ready = ready_tx.clone();
        let (next_go_tx, next_go_rx) = mpsc::channel::<()>();
        let next = thread::spawn(move || {
            next_ready.send(()).unwrap();
            next_go_rx
                .recv_timeout(WAIT_LIMIT)
                .expect("bounded next gate");
            next_store.queue_next("worker").unwrap()
        });
        drop(ready_tx);
        let ready =
            ready_rx.recv_timeout(WAIT_LIMIT).is_ok() && ready_rx.recv_timeout(WAIT_LIMIT).is_ok();
        let _ = admission_go_tx.send(());
        let _ = next_go_tx.send(());
        admission.join().expect("admission actor completed");
        let next = next.join().expect("next actor completed");
        assert!(ready, "attempt {attempt}: both actors reached bounded start gate");
        let events = store.events(1000).unwrap();
        let admitted_seq = events
            .iter()
            .find(|event| event.kind == "queue.admitted" && event.payload["item_id"] == item.item_id)
            .unwrap()
            .seq;
        let item_row = store.queue_item(&item.item_id).unwrap();
        if let Some(claimed) = next.claimed {
            assert_eq!(claimed.item_id, item.item_id);
            assert_eq!(
                item_row.state,
                QueueState::Claimed,
                "attempt {attempt}: a successful next must leave the item claimed"
            );
            let claimed_seq = events
                .iter()
                .find(|event| event.kind == "queue.claimed" && event.payload["item_id"] == item.item_id)
                .unwrap()
                .seq;
            assert!(
                admitted_seq < claimed_seq,
                "attempt {attempt}: admission must linearize before claim"
            );
        } else {
            assert_eq!(
                item_row.state,
                QueueState::Ready,
                "attempt {attempt}: if next did not claim, the item must still be admitted"
            );
            assert!(
                item_row.claim.is_none(),
                "attempt {attempt}: Ready item must have no claim"
            );
        }
        assert!(!events.iter().any(|event| {
            event.kind == "queue.claimed"
                && event.payload["item_id"] == item.item_id
                && event.seq < admitted_seq
        }));
        assert!(setup.queue_audit(None).unwrap().ok);
    }
}
