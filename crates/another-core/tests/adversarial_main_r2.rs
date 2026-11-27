//! Adversarial pass R2 on `main` at `fa0ef04`: text chosen by another agent or a paired node
//! that reaches a person's terminal. #146 (W1, W2) made mail subjects one line of plain
//! text. Other free-text fields still arrive and are printed unchanged. `gap_*` tests state
//! the expected behaviour and fail on `main`; `control_*` tests pin what should stay.

use std::path::Path;

use another_core::test_support::forge_event;
use another_core::*;
use rusqlite::Connection;
use serde_json::Value;

/// OSC 52 (write the clipboard) followed by a clear-screen: harmless to print in a test log
/// only because `{:?}` escapes it.
const EVIL: &str = "fine\u{1b}]52;c;ZXZpbA==\u{7}\u{1b}[2J";

fn unsafe_in(text: &str) -> bool {
    text.chars().any(is_unsafe_text_char)
}

struct Node {
    _dir: tempfile::TempDir,
    store: Store,
    home: std::path::PathBuf,
    project: String,
}

fn node(name: &str) -> Node {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let store = Store::open(&home).unwrap();
    store.set_node_name(name).unwrap();
    let project = dir.path().join("shop");
    std::fs::create_dir_all(&project).unwrap();
    let project = project.to_string_lossy().into_owned();
    store.ensure_project("shop", &project).unwrap();
    Node {
        _dir: dir,
        store,
        home,
        project,
    }
}

fn register(n: &Node, name: &str, description: Option<&str>) -> Result<Agent> {
    n.store.register_agent(RegisterAgent {
        name: name.into(),
        program: Program::Claude,
        project: Some(n.project.clone()),
        model: None,
        description: description.map(Into::into),
    })
}

fn hold(n: &Node, agent: &str, path: &str, reason: Option<&str>) -> Result<HoldReport> {
    n.store.acquire_holds(AcquireHolds {
        agent: agent.into(),
        project: n.project.clone(),
        paths: vec![path.into()],
        ttl_seconds: Some(600),
        exclusive: true,
        reason: reason.map(Into::into),
        repo_wide: false,
    })
}

fn pair(hub: &Node, spoke: &Node) {
    test_support::pair(&hub.store, &spoke.store, &Perm::ALL);
}

/// Everything `spoke` authored, with `edit` applied to each payload and re-signed with the
/// spoke's own key: a correctly signed event the spoke really sent.
fn resigned(spoke: &Node, edit: impl Fn(&str, &mut Value)) -> Vec<WireEvent> {
    let c = Connection::open(spoke.home.join(DB_FILE)).unwrap();
    let (events, _) = spoke.store.outbound_events(0, 10_000).unwrap();
    events
        .into_iter()
        .map(|mut e| {
            let mut p: Value = serde_json::from_str(&e.payload).unwrap();
            edit(&e.kind, &mut p);
            e.payload = p.to_string();
            forge_event(&c, e)
        })
        .collect()
}

fn feed_text(store: &Store) -> String {
    store
        .activity(ActivityQuery {
            since: None,
            limit: 500,
            kinds: vec![],
        })
        .unwrap()
        .items
        .iter()
        .map(|i| i.summary.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

/// R1. A hold reason is free text from the acting agent: any local agent, or a remote token
/// through `hold_paths`. It is printed by `another holds`, the board, the dashboard and the
/// edit hook's denial, and it reaches peers. Like a subject, it should be one line.
#[test]
fn gap_a_hold_reason_with_terminal_escapes_is_refused() {
    let n = node("solo");
    register(&n, "alice", None).unwrap();
    let r = hold(&n, "alice", "src/a.rs", Some(EVIL));
    let stored = n
        .store
        .holds(HoldFilter {
            project: None,
            agent: None,
        })
        .unwrap();
    assert!(
        r.is_err()
            || stored
                .iter()
                .all(|h| !h.reason.as_deref().is_some_and(unsafe_in)),
        "hold reason stored with escapes: {:?}",
        stored.iter().map(|h| &h.reason).collect::<Vec<_>>()
    );
}

/// R2. An agent description is printed by `another agent list` and shown to other agents.
#[test]
fn gap_an_agent_description_with_terminal_escapes_is_refused() {
    let n = node("solo");
    let r = register(&n, "alice", Some(EVIL));
    let listed = n.store.agents().unwrap();
    assert!(
        r.is_err()
            || listed
                .iter()
                .all(|a| !a.description.as_deref().is_some_and(unsafe_in)),
        "description stored with escapes: {:?}",
        listed.iter().map(|a| &a.description).collect::<Vec<_>>()
    );
}

/// R3. A paired node's hold reason and pattern reach this node's activity feed (and so
/// `another activity` and the session digest) unchanged.
#[test]
fn gap_a_peer_hold_with_terminal_escapes_never_reaches_the_feed() {
    let hub = node("hub");
    let spoke = node("spoke");
    pair(&hub, &spoke);
    register(&spoke, "bob", None).unwrap();
    hold(&spoke, "bob", "src/b.rs", Some("clean")).unwrap();
    let events = resigned(&spoke, |kind, p| {
        if kind == "hold.acquired" {
            p["reason"] = Value::String(EVIL.into());
            p["pattern"] = Value::String(format!("src/{EVIL}.rs"));
        }
    });
    let report = hub
        .store
        .ingest(
            spoke.store.node_id(),
            &events,
            &[spoke.store.node_entry().unwrap()],
        )
        .unwrap();
    let text = feed_text(&hub.store);
    let stored: Vec<(String, Option<String>)> = Connection::open(hub.home.join(DB_FILE))
        .unwrap()
        .prepare("SELECT pattern, reason FROM holds WHERE node_id = ?1")
        .unwrap()
        .query_map([spoke.store.node_id()], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    // `feed_text` joins summaries with '\n', itself an unsafe character: check each line.
    // The reason is free text and is stored as one safe line. The pattern names a file and
    // is matched against local holds, so it is kept as sent (as a local one is, D75);
    // `another holds` cleans it when printing (D97).
    assert!(
        !text.lines().any(unsafe_in)
            && stored.iter().all(
                |(p, r)| p == &format!("src/{EVIL}.rs") && !r.as_deref().is_some_and(unsafe_in)
            ),
        "ingest report {report:?}; feed {text:?}; stored {stored:?}"
    );
}

// R4 (a peer agent's description and model) was checked through `Store::agents()`, which
// lists local agents only, so it could not fail (FINDINGS-178 P2-test). review178.rs reads
// the imported rows instead.

/// Control: #146 holds. A subject with escapes is refused locally.
#[test]
fn control_a_subject_with_escapes_is_still_refused() {
    let n = node("solo");
    register(&n, "alice", None).unwrap();
    register(&n, "bob", None).unwrap();
    let err = n
        .store
        .send_message(SendMessage {
            from: "alice".into(),
            to: vec!["bob".into()],
            subject: EVIL.into(),
            body: "b".into(),
            ..Default::default()
        })
        .unwrap_err();
    assert_eq!(err.code(), "invalid");
}

/// Control: an ordinary peer hold still arrives and shows in the feed.
#[test]
fn control_a_clean_peer_hold_still_syncs() {
    let hub = node("hub");
    let spoke = node("spoke");
    pair(&hub, &spoke);
    register(&spoke, "bob", None).unwrap();
    hold(&spoke, "bob", "src/b.rs", Some("refactor")).unwrap();
    let events = resigned(&spoke, |_, _| {});
    let report = hub
        .store
        .ingest(
            spoke.store.node_id(),
            &events,
            &[spoke.store.node_entry().unwrap()],
        )
        .unwrap();
    assert!(report.rejected.is_empty(), "{report:?}");
    assert!(feed_text(&hub.store).contains("src/b.rs"));
    let _ = Path::new("");
}
