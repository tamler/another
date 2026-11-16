//! Adversarial review of #178 (head 373b98a). #178 cleans a peer's names, reasons, models,
//! descriptions and resource text on import, but keeps project slugs as given. An imported
//! agent's address is `name@slug@node`, and a hold lists its project by slug, so a peer's
//! slug puts control characters into what `another holds` (and inbox, thread) print.

use another_core::*;
use rusqlite::Connection;
use serde_json::Value;

/// OSC 52 (write the clipboard) and clear-screen.
const EVIL: &str = "shop\u{1b}]52;c;ZXZpbA==\u{7}\u{1b}[2J";

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

fn pair(hub: &Node, spoke: &Node) {
    let code = hub
        .store
        .create_invite("https://hub.example", &Perm::ALL, 3600)
        .unwrap();
    let (invite, req) = spoke.store.prepare_join(&code).unwrap();
    let resp = hub.store.accept_pairing(&req).unwrap();
    spoke.store.complete_join(&invite, &resp).unwrap();
}

/// Everything `spoke` authored, with `edit` applied and re-signed with the spoke's key.
fn resigned(spoke: &Node, edit: impl Fn(&str, &mut Value)) -> Vec<WireEvent> {
    let secret: String = Connection::open(spoke.home.join(DB_FILE))
        .unwrap()
        .query_row("SELECT secret_key FROM nodes WHERE is_local = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    let key = crypto::decode_secret(&secret).unwrap();
    let (events, _) = spoke.store.outbound_events(0, 10_000).unwrap();
    events
        .into_iter()
        .map(|mut e| {
            let mut p: Value = serde_json::from_str(&e.payload).unwrap();
            edit(&e.kind, &mut p);
            e.payload = p.to_string();
            e.sig = crypto::sign(
                &key,
                &crypto::event_message(
                    &e.event_id,
                    &e.node_id,
                    e.ts,
                    &e.actor,
                    &e.kind,
                    &e.payload,
                ),
            );
            e
        })
        .collect()
}

fn spoke_with_agent_and_hold(hub: &Node) -> Node {
    let spoke = node("spoke");
    pair(hub, &spoke);
    spoke
        .store
        .register_agent(RegisterAgent {
            name: "bob".into(),
            program: Program::Claude,
            project: Some(spoke.project.clone()),
            model: None,
            description: None,
        })
        .unwrap();
    spoke
        .store
        .acquire_holds(AcquireHolds {
            agent: "bob".into(),
            project: spoke.project.clone(),
            paths: vec!["src/a.rs".into()],
            ttl_seconds: Some(600),
            exclusive: true,
            reason: None,
            repo_wide: false,
        })
        .unwrap();
    spoke
}

/// Addresses of agents the hub imported from `spoke`. `Store::agents()` lists only local
/// agents, so read the projection directly.
fn peer_agents(hub: &Node, spoke: &Node, column: &str) -> Vec<Option<String>> {
    Connection::open(hub.home.join(DB_FILE))
        .unwrap()
        .prepare(&format!("SELECT {column} FROM agents WHERE node_id = ?1"))
        .unwrap()
        .query_map([spoke.store.node_id()], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn a_peer_project_slug_does_not_put_escapes_into_an_address() {
    let hub = node("hub");
    let spoke = spoke_with_agent_and_hold(&hub);
    let events = resigned(&spoke, |kind, p| {
        if kind == "project.ensured" {
            p["slug"] = Value::String(EVIL.into());
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
    assert!(report.rejected.is_empty(), "{report:?}");
    let addresses = peer_agents(&hub, &spoke, "address");
    assert_eq!(addresses.len(), 1, "bob was imported");
    assert!(
        !addresses.iter().flatten().any(|a| unsafe_in(a)),
        "a peer's slug puts escapes into a stored address: {addresses:?}"
    );
}

/// The PR's `gap_a_peer_agent_with_terminal_escapes_is_not_listed_raw` checks
/// `hub.store.agents()`, which has no peer agents in it, so it cannot fail. This is the
/// same check on the imported rows.
#[test]
fn a_peer_agent_description_and_model_are_cleaned_on_import() {
    let hub = node("hub");
    let spoke = spoke_with_agent_and_hold(&hub);
    let events = resigned(&spoke, |kind, p| {
        if kind == "agent.registered" {
            p["description"] = Value::String(EVIL.into());
            p["model"] = Value::String(EVIL.into());
        }
    });
    hub.store
        .ingest(
            spoke.store.node_id(),
            &events,
            &[spoke.store.node_entry().unwrap()],
        )
        .unwrap();
    assert!(
        hub.store.agents().unwrap().iter().all(|a| a.name != "bob"),
        "agents() lists only local agents (why the PR's test is vacuous)"
    );
    let stored: Vec<Option<String>> = peer_agents(&hub, &spoke, "description")
        .into_iter()
        .chain(peer_agents(&hub, &spoke, "model"))
        .collect();
    assert_eq!(stored.len(), 2, "bob was imported");
    assert!(
        !stored.iter().flatten().any(|t| unsafe_in(t)),
        "stored {stored:?}"
    );
}
