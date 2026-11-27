use another_core::test_support::forge_event;
use another_core::*;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

const T0: i64 = 1_800_000_000_000;

struct Node {
    store: Store,
    db: std::path::PathBuf,
}

impl Node {
    fn new(root: &std::path::Path, name: &str) -> Self {
        let home = root.join(name);
        let (clock, _) = Clock::manual(T0);
        let store = Store::open_with_clock(&home, clock).unwrap();
        store.set_node_name(name).unwrap();
        Self {
            store,
            db: home.join(DB_FILE),
        }
    }

    fn sql(&self) -> Connection {
        Connection::open(&self.db).unwrap()
    }

    fn agent(&self, name: &str) -> String {
        self.store
            .register_agent(RegisterAgent {
                name: name.into(),
                program: Program::Codex,
                project: None,
                model: None,
                description: None,
            })
            .unwrap()
            .agent_id
    }

    fn event(&self, id: &str, kind: &str, payload: Value) -> WireEvent {
        forge_event(
            &self.sql(),
            WireEvent {
                seq: 1,
                event_id: id.into(),
                node_id: self.store.node_id().into(),
                ts: T0,
                actor: "peer".into(),
                kind: kind.into(),
                payload: payload.to_string(),
                prev_id: None,
                sig: String::new(),
            },
        )
    }

    fn ingest(&self, peer: &Node, events: &[WireEvent]) -> IngestReport {
        self.store
            .ingest(
                peer.store.node_id(),
                events,
                &[peer.store.node_entry().unwrap()],
            )
            .unwrap()
    }

    fn audits(&self) -> Vec<(String, String)> {
        self.sql().prepare("SELECT outcome, detail FROM audit_log WHERE operation = 'federation hold ownership' ORDER BY rowid")
            .unwrap().query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
            .collect::<rusqlite::Result<_>>().unwrap()
    }

    fn project(&self, root: &std::path::Path, name: &str) -> Project {
        let path = root.join(name);
        std::fs::create_dir_all(&path).unwrap();
        self.store
            .ensure_project("test", path.to_str().unwrap())
            .unwrap()
    }

    fn push_to(&self, hub: &Node) {
        let (events, _) = self.store.outbound_events(0, 10_000).unwrap();
        assert!(hub.ingest(self, &events).rejected.is_empty());
    }
}

fn pair(hub: &Node, peer: &Node) {
    let code = hub
        .store
        .create_invite("https://hub.example", &Perm::ALL, 3600)
        .unwrap();
    let (invite, req) = peer.store.prepare_join(&code).unwrap();
    let resp = hub.store.accept_pairing(&req).unwrap();
    peer.store.complete_join(&invite, &resp).unwrap();
}

#[test]
fn refused_hold_replay_is_audited_each_time_and_batch_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let peer = Node::new(dir.path(), "peer");
    pair(&hub, &peer);
    let alice = hub.agent("alice");
    let ev = peer.event(
        "attack",
        "hold.acquired",
        json!({"hold_id":"fresh", "agent_id":alice}),
    );
    for _ in 0..2 {
        let report = hub.ingest(&peer, std::slice::from_ref(&ev));
        assert_eq!(report.rejected.len(), 1);
        assert_eq!(report.hold_refused, 1);
    }
    let audits = hub.audits();
    assert_eq!(audits.len(), 2);
    assert!(
        audits[0].1.contains("attack")
            && audits[0].1.contains("agent")
            && audits[0].1.contains("hub")
    );
    let events: Vec<_> = (0..500)
        .map(|i| {
            peer.event(
                &format!("attack-{i}"),
                "hold.updated",
                json!({"hold_id":"fresh", "agent_id":alice}),
            )
        })
        .collect();
    let report = hub.ingest(&peer, &events);
    assert_eq!(report.rejected.len(), 500);
    assert_eq!(report.hold_refused, 500);
    assert_eq!(hub.audits().len(), 23);
    assert!(hub.audits().last().unwrap().1.contains("480"));
    assert_eq!(
        hub.sql()
            .query_row(
                "SELECT count(*) FROM events WHERE event_id LIKE 'attack%'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn inconsistent_row_is_inert_and_kept() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let peer = Node::new(dir.path(), "peer");
    pair(&hub, &peer);
    let alice = hub.agent("alice");
    hub.agent("bob");
    let root = dir.path().join("project");
    std::fs::create_dir(&root).unwrap();
    let project = hub
        .store
        .ensure_project("alice", root.to_str().unwrap())
        .unwrap();
    // A pre-authority build could keep an unclamped TTL. This row is already due for
    // keepalive, even though its expiry is ten years away.
    hub.sql().execute("INSERT INTO holds (hold_id, node_id, project_id, agent_id, pattern, exclusive, ttl_seconds, created_at, renewed_at, expires_at) VALUES ('legacy', ?1, ?2, ?3, 'file.rs', 1, 1000000000, ?4, ?4, ?5)", params![peer.store.node_id(), project.project_id, alice, T0, T0 + 315_360_000_000_i64]).unwrap();
    assert!(hub
        .store
        .release_holds("alice", &["legacy".into()])
        .is_err());
    assert!(hub.store.renew_holds("alice", &["legacy".into()]).is_err());
    assert_eq!(
        hub.store
            .keepalive("alice", &SessionInfo::default())
            .unwrap(),
        0
    );
    hub.store.renew_holds("alice", &[]).unwrap();
    hub.store.heartbeat("alice", false).unwrap();
    hub.store.heartbeat("alice", true).unwrap();
    assert!(hub
        .store
        .release_pattern("alice", root.to_str().unwrap(), "file.rs")
        .unwrap()
        .released
        .is_empty());
    assert_eq!(hub.store.inconsistent_holds(5).unwrap().0, 1);
    let guard = hub
        .store
        .guard_edit("alice", root.to_str().unwrap(), &["file.rs".into()], "test")
        .unwrap();
    assert!(guard.covered.is_empty());
    assert!(!guard.held.is_empty());
    assert!(guard.held.iter().all(|h| h.hold_id != "legacy"));
    hub.store.release_holds("alice", &[]).unwrap();
    let held = hub
        .store
        .acquire_holds(AcquireHolds {
            agent: "bob".into(),
            project: root.to_string_lossy().into_owned(),
            paths: vec!["file.rs".into()],
            exclusive: true,
            reason: None,
            ttl_seconds: None,
            repo_wide: false,
        })
        .unwrap();
    assert!(held.conflicts.is_empty());
    hub.store.release_holds("bob", &[]).unwrap();
    let acquired = hub
        .store
        .acquire_holds(AcquireHolds {
            agent: "alice".into(),
            project: root.to_string_lossy().into_owned(),
            paths: vec!["file.rs".into()],
            exclusive: true,
            reason: None,
            ttl_seconds: None,
            repo_wide: false,
        })
        .unwrap();
    assert!(acquired.granted.iter().all(|h| h.hold_id != "legacy"));
    // A foreign agent attached to a local project is equally inert as a blocker.
    let foreign = peer.agent("foreign");
    peer.push_to(&hub);
    hub.sql()
        .execute(
            "UPDATE holds SET agent_id = ?1 WHERE hold_id = 'legacy'",
            [&foreign],
        )
        .unwrap();
    hub.store.release_holds("alice", &[]).unwrap();
    assert!(hub
        .store
        .guard_edit("bob", root.to_str().unwrap(), &["file.rs".into()], "test")
        .unwrap()
        .conflicts
        .is_empty());
    let unchanged: (i64, Option<i64>) = hub
        .sql()
        .query_row(
            "SELECT renewed_at, released_at FROM holds WHERE hold_id = 'legacy'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(unchanged, (T0, None));
    let local_events: i64 = hub.sql().query_row("SELECT count(*) FROM events WHERE node_id = ?1 AND kind IN ('hold.updated', 'hold.renewed', 'hold.released') AND payload LIKE '%legacy%'", [hub.store.node_id()], |r| r.get(0)).unwrap();
    assert_eq!(local_events, 0);
    let ev = peer.event(
        "release-legacy",
        "hold.released",
        json!({"hold_ids":["legacy"]}),
    );
    assert_eq!(hub.ingest(&peer, &[ev]).accepted, 1);
    assert!(hub
        .sql()
        .query_row(
            "SELECT released_at FROM holds WHERE hold_id = 'legacy'",
            [],
            |r| r.get::<_, Option<i64>>(0)
        )
        .unwrap()
        .is_some());
}

#[test]
fn project_agent_and_hold_collisions_name_the_owner() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let alpha = Node::new(dir.path(), "alpha");
    let peer = Node::new(dir.path(), "peer");
    pair(&hub, &alpha);
    pair(&hub, &peer);
    let local_agent = hub.agent("local");
    let local_project = hub.project(dir.path(), "local-project");
    let agent = peer.agent("spy");
    let project = peer.project(dir.path(), "peer-project");
    peer.push_to(&hub);
    let third_agent = alpha.agent("alice");
    let third_project = alpha.project(dir.path(), "alpha-project");
    alpha.push_to(&hub);
    for (id, aid, pid, object) in [
        (
            "local-both",
            local_agent.clone(),
            local_project.project_id.clone(),
            "agent",
        ),
        (
            "local-project",
            agent.clone(),
            local_project.project_id.clone(),
            "project",
        ),
        (
            "third-agent",
            third_agent.clone(),
            third_project.project_id.clone(),
            "agent",
        ),
    ] {
        let ev = peer.event(
            id,
            "hold.acquired",
            json!({"hold_id":id, "agent_id":aid, "project_id":pid, "pattern":"a.rs"}),
        );
        let report = hub.ingest(&peer, &[ev]);
        assert_eq!(report.hold_refused, 1);
        assert!(report.rejected[0].reason.contains(object));
        assert_eq!(
            hub.sql()
                .query_row("SELECT count(*) FROM holds WHERE hold_id = ?1", [id], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    for (owner, aid, pid, id) in [
        (&hub, local_agent, local_project.project_id, "hub-hold"),
        (&alpha, third_agent, third_project.project_id, "alpha-hold"),
    ] {
        let own = owner.event(id, "hold.acquired", json!({"hold_id":id, "agent_id":aid, "project_id":pid, "pattern":"a.rs", "reason":"original", "ttl_seconds":900}));
        // Hub events cannot enter through ingest; seed the equivalent local hold row.
        if owner.store.node_id() == hub.store.node_id() {
            hub.sql().execute("INSERT INTO holds (hold_id,node_id,project_id,agent_id,pattern,exclusive,reason,ttl_seconds,created_at,renewed_at,expires_at) VALUES (?1,?2,?3,?4,'a.rs',1,'original',900,?5,?5,?6)", params![id,hub.store.node_id(),pid,aid,T0,T0+900_000]).unwrap();
        } else {
            assert_eq!(hub.ingest(owner, &[own]).accepted, 1);
        }
        let before: String = hub.sql().query_row("SELECT json_array(node_id,agent_id,exclusive,reason,expires_at) FROM holds WHERE hold_id = ?1", [id], |r| r.get(0)).unwrap();
        let ev = peer.event(&format!("steal-{id}"), "hold.updated", json!({"hold_id":id, "agent_id":agent, "project_id":project.project_id, "pattern":"a.rs", "exclusive":false, "reason":"stolen"}));
        assert_eq!(hub.ingest(&peer, &[ev]).hold_refused, 1);
        let after: String = hub.sql().query_row("SELECT json_array(node_id,agent_id,exclusive,reason,expires_at) FROM holds WHERE hold_id = ?1", [id], |r| r.get(0)).unwrap();
        assert_eq!(before, after);
        let audit = hub.audits().last().unwrap().1.clone();
        assert!(audit.contains(id) && audit.contains(owner.store.node_id()));
    }
}

#[test]
fn lifecycle_ignores_foreign_ids_but_applies_own_and_duplicates_are_quiet() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Node::new(dir.path(), "hub");
    let alpha = Node::new(dir.path(), "alpha");
    let peer = Node::new(dir.path(), "peer");
    pair(&hub, &alpha);
    pair(&hub, &peer);
    for (node, id) in [(&alpha, "alpha-hold"), (&peer, "peer-hold")] {
        let agent = node.agent("alice");
        let project = node.project(dir.path(), id);
        node.push_to(&hub);
        let ev = node.event(id, "hold.acquired", json!({"hold_id":id,"agent_id":agent,"project_id":project.project_id,"pattern":"a.rs","ttl_seconds":900}));
        assert_eq!(hub.ingest(node, std::slice::from_ref(&ev)).accepted, 1);
        assert_eq!(hub.ingest(node, &[ev]).duplicates, 1);
    }
    assert!(hub.audits().is_empty());
    hub.sql()
        .execute(
            "UPDATE holds SET renewed_at = ?1, expires_at = ?1 + 60000",
            [T0 - 10_000],
        )
        .unwrap();
    for kind in ["hold.renewed", "hold.released", "hold.expired"] {
        let ev = peer.event(
            kind,
            kind,
            json!({"hold_ids":["peer-hold","alpha-hold"],"hold_id":"alpha-hold"}),
        );
        let report = hub.ingest(&peer, std::slice::from_ref(&ev));
        assert_eq!(report.accepted, 1);
        assert_eq!(report.not_applied.len(), 1);
        assert_eq!(report.hold_refused, 0);
        assert_eq!(hub.ingest(&peer, &[ev]).duplicates, 1);
        let row: (i64, i64, Option<i64>) = hub
            .sql()
            .query_row(
                "SELECT renewed_at,expires_at,released_at FROM holds WHERE hold_id='alpha-hold'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row, (T0 - 10_000, T0 + 50_000, None));
        if kind == "hold.renewed" {
            assert_eq!(
                hub.sql()
                    .query_row(
                        "SELECT renewed_at FROM holds WHERE hold_id='peer-hold'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )
                    .unwrap(),
                T0
            );
        }
    }
    assert_eq!(hub.audits().len(), 3);
    assert!(hub
        .audits()
        .iter()
        .all(|(outcome, detail)| outcome == "ignored" && detail.contains("alpha-hold")));
    // The honest owner's lifecycle applies at the hub and through a hub pull.
    for (i, kind) in [
        "hold.updated",
        "hold.renewed",
        "hold.released",
        "hold.expired",
    ]
    .iter()
    .enumerate()
    {
        let agent = alpha.store.agents().unwrap()[0].agent_id.clone();
        let project = alpha.store.projects().unwrap()[0].project_id.clone();
        let ev = alpha.event(&format!("own-{i}"), kind, json!({"hold_id":"alpha-hold","hold_ids":["alpha-hold"],"agent_id":agent,"project_id":project,"pattern":"a.rs","exclusive":false,"reason":"owner","ttl_seconds":120}));
        let report = hub.ingest(&alpha, &[ev]);
        assert_eq!(report.accepted, 1);
        assert!(report.not_applied.is_empty());
    }
    let pulled = hub.store.pull_for(peer.store.node_id(), 0, 10_000).unwrap();
    let report = peer
        .store
        .ingest(hub.store.node_id(), &pulled.events, &pulled.nodes)
        .unwrap();
    assert!(report.rejected.is_empty());
    assert!(peer
        .sql()
        .query_row(
            "SELECT released_at FROM holds WHERE hold_id='alpha-hold'",
            [],
            |r| r.get::<_, Option<i64>>(0)
        )
        .unwrap()
        .is_some());
    assert_eq!(hub.audits().len(), 3);
}

#[test]
fn ingest_report_accepts_both_wire_versions() {
    #[derive(serde::Deserialize)]
    struct OldReport {
        accepted: usize,
        duplicates: usize,
        rejected: Vec<Rejected>,
    }
    let old = json!({"accepted":1,"duplicates":2,"rejected":[]});
    let new: IngestReport = serde_json::from_value(old).unwrap();
    assert_eq!(new.hold_refused, 0);
    let old: OldReport = serde_json::from_value(
        serde_json::to_value(IngestReport {
            hold_refused: 7,
            ..new
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        (old.accepted, old.duplicates, old.rejected.len()),
        (1, 2, 0)
    );
}
