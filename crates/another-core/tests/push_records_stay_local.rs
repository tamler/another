//! A node's wake records name local terminals (Herdr panes, tmux, cmux) and local
//! message ids. They are evidence for that node's `another stats` and never leave it: a
//! spoke does not offer them, a hub refuses them from a modified spoke, and a hub that
//! stored one under an older build does not relay it.
//!
//! Current builds write `transport.wake` (and `transport.gone`). Older builds wrote
//! `push.woke`, `push.notified` and `push.suppressed`; those prefixes stay never-shared
//! as well. The tests below use the current API; classify unit tests in `federation.rs`
//! pin the legacy kinds.

use another_core::*;
use rusqlite::Connection;

fn node(dir: &std::path::Path, name: &str) -> Store {
    let (clock, _) = Clock::manual(1_800_000_000_000);
    let s = Store::open_with_clock(&dir.join(name), clock).unwrap();
    s.set_node_name(name).unwrap();
    s
}

fn pair(hub: &Store, spoke: &Store) {
    let code = hub
        .create_invite("https://hub.example", &Perm::ALL, 3600)
        .unwrap();
    let (invite, req) = spoke.prepare_join(&code).unwrap();
    let resp = hub.accept_pairing(&req).unwrap();
    spoke.complete_join(&invite, &resp).unwrap();
}

/// Hub, alpha and beta paired with every permission; carol on alpha has a recorded wake.
fn woken(dir: &std::path::Path) -> (Store, Store, Store) {
    let hub = node(dir, "hub");
    let alpha = node(dir, "alpha");
    let beta = node(dir, "beta");
    pair(&hub, &alpha);
    pair(&hub, &beta);
    alpha
        .register_agent(RegisterAgent {
            name: "carol".into(),
            program: Program::Claude,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    alpha
        .record_transport_wake(&TransportWake {
            agent: "carol".into(),
            session_id: None,
            transport: "herdr".into(),
            target: "secret-session:%7".into(),
            outcome: WakeOutcome::Started,
            detail: "herdr: agent_prompted (working)".into(),
            kind: WakeKind::Mail,
            message_ids: Vec::new(),
            trigger: "daemon".into(),
        })
        .unwrap();
    (hub, alpha, beta)
}

/// Alpha's stored `transport.wake`, exactly as signed.
fn stored_wake(dir: &std::path::Path) -> WireEvent {
    Connection::open(dir.join("alpha").join(DB_FILE))
        .unwrap()
        .query_row(
            "SELECT seq, event_id, node_id, ts, actor, kind, payload, prev_id, sig
             FROM events WHERE kind = 'transport.wake'",
            [],
            |r| {
                Ok(WireEvent {
                    seq: r.get(0)?,
                    event_id: r.get(1)?,
                    node_id: r.get(2)?,
                    ts: r.get(3)?,
                    actor: r.get(4)?,
                    kind: r.get(5)?,
                    payload: r.get(6)?,
                    prev_id: r.get(7)?,
                    sig: r.get(8)?,
                })
            },
        )
        .unwrap()
}

#[test]
fn gap_a_wake_record_does_not_leave_its_node() {
    let dir = tempfile::tempdir().unwrap();
    let (hub, alpha, beta) = woken(dir.path());
    let (out, _) = alpha.outbound_events(0, 10_000).unwrap();
    assert!(
        out.iter().all(|e| !e.kind.starts_with("push.")
            && e.kind != "transport.wake"
            && e.kind != "transport.gone"),
        "alpha publishes its wake record: {:?}",
        out.iter().map(|e| &e.kind).collect::<Vec<_>>()
    );
    hub.ingest(alpha.node_id(), &out, &[alpha.node_entry().unwrap()])
        .unwrap();
    let page = hub.pull_for(beta.node_id(), 0, 10_000).unwrap();
    assert!(
        page.events
            .iter()
            .all(|e| !e.payload.contains("secret-session")),
        "beta receives alpha's terminal target"
    );
}

/// A modified spoke that pushes its correctly signed wake record is refused.
#[test]
fn a_hub_refuses_a_pushed_wake_record() {
    let dir = tempfile::tempdir().unwrap();
    let (hub, alpha, _) = woken(dir.path());
    let wake = stored_wake(dir.path());
    let report = hub
        .ingest(
            alpha.node_id(),
            std::slice::from_ref(&wake),
            &[alpha.node_entry().unwrap()],
        )
        .unwrap();
    assert_eq!(report.accepted, 0, "{report:?}");
    assert_eq!(report.rejected.len(), 1, "{report:?}");
    assert!(
        report.rejected[0]
            .reason
            .contains("transport.wake events are not shared"),
        "{report:?}"
    );
}

/// A hub that stored a wake record under an older build keeps it to itself: relaying is
/// decided when the hub sends, not when it stored the event.
#[test]
fn a_wake_record_stored_by_an_older_hub_is_not_relayed() {
    let dir = tempfile::tempdir().unwrap();
    let (hub, alpha, beta) = woken(dir.path());
    let (out, _) = alpha.outbound_events(0, 10_000).unwrap();
    hub.ingest(alpha.node_id(), &out, &[alpha.node_entry().unwrap()])
        .unwrap();
    let wake = stored_wake(dir.path());
    Connection::open(dir.path().join("hub").join(DB_FILE))
        .unwrap()
        .execute(
            "INSERT INTO events (event_id, node_id, ts, actor, kind, payload, prev_id, sig)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                wake.event_id,
                wake.node_id,
                wake.ts,
                wake.actor,
                wake.kind,
                wake.payload,
                wake.prev_id,
                wake.sig
            ],
        )
        .unwrap();
    let page = hub.pull_for(beta.node_id(), 0, 10_000).unwrap();
    assert!(
        page.events.iter().all(|e| e.event_id != wake.event_id),
        "the hub relayed a stored wake record"
    );
}
