//! Adversarial review of #180 (head da227fe). The 4096-byte limit is checked in
//! `normalize_path`, on the way in from local callers. A paired node's hold pattern is
//! stored on import without that check, and `remote_clashes` compiles it (through
//! `patterns_overlap`) on every local acquire, check and edit guard in a project with the
//! same slug.

use another_core::*;
use rusqlite::Connection;
use serde_json::Value;

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
    std::fs::create_dir_all(project.join("src")).unwrap();
    let project = project.to_string_lossy().into_owned();
    store.ensure_project("shop", &project).unwrap();
    store
        .register_agent(RegisterAgent {
            name: format!("{name}-agent"),
            program: Program::Claude,
            project: Some(project.clone()),
            model: None,
            description: None,
        })
        .unwrap();
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

fn hold(n: &Node, agent: &str, path: &str) -> Result<HoldReport> {
    n.store.acquire_holds(AcquireHolds {
        agent: agent.into(),
        project: n.project.clone(),
        paths: vec![path.into()],
        ttl_seconds: Some(600),
        exclusive: true,
        reason: None,
        repo_wide: false,
    })
}

fn outcome<T: std::fmt::Debug>(f: impl FnOnce() -> Result<T>) -> String {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(Ok(v)) => format!(
            "ok: {}",
            format!("{v:?}").chars().take(120).collect::<String>()
        ),
        Ok(Err(e)) => format!(
            "err: {}",
            e.to_string().chars().take(120).collect::<String>()
        ),
        Err(_) => "PANIC".to_string(),
    }
}

#[test]
fn a_peer_hold_pattern_over_the_limit_cannot_panic_local_hold_checks() {
    let hub = node("hub");
    let spoke = node("spoke");
    pair(&hub, &spoke);
    hold(&spoke, "spoke-agent", "src/*.rs").unwrap();
    let huge = format!("src/{}*.rs", "a".repeat(500_000));
    let events = resigned(&spoke, |kind, p| {
        if kind == "hold.acquired" {
            p["pattern"] = Value::String(huge.clone());
        }
    });
    let ingest = outcome(|| {
        hub.store.ingest(
            spoke.store.node_id(),
            &events,
            &[spoke.store.node_entry().unwrap()],
        )
    });
    let stored: Option<usize> = Connection::open(hub.home.join(DB_FILE))
        .unwrap()
        .query_row(
            "SELECT max(length(pattern)) FROM holds WHERE node_id = ?1",
            [spoke.store.node_id()],
            |r| r.get(0),
        )
        .unwrap();
    let acquire = outcome(|| hold(&hub, "hub-agent", "src/x.rs"));
    let guard = outcome(|| {
        hub.store.guard_edit(
            "hub-agent",
            &hub.project,
            &["src/y.rs".into()],
            "editing (auto)",
        )
    });
    println!(
        "ingest: {ingest}\nstored peer pattern bytes: {stored:?}\nacquire: {acquire}\nguard_edit: {guard}"
    );
    assert!(
        stored.unwrap_or(0) <= MAX_HOLD_PATH_BYTES,
        "a peer pattern of {stored:?} bytes was stored past the {MAX_HOLD_PATH_BYTES}-byte limit"
    );
    assert!(
        acquire != "PANIC" && guard != "PANIC",
        "acquire: {acquire}; guard: {guard}"
    );
}
