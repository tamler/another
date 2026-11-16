//! The dashboard on a hub that also shows other nodes' work: releasing another node's hold
//! is refused and leaves it as it is, and a local hold that overlaps another node's hold in
//! a project of the same name shows that hold as a remote warning, not a conflict. Also the
//! dashboard's refusals for a request that is already closed or unknown. (Requests never
//! leave their node, so there is no foreign request to refuse.)

use another_core::*;
use rusqlite::Connection;

struct Node {
    _dir: tempfile::TempDir,
    store: Store,
    db: std::path::PathBuf,
}

impl Node {
    fn new(name: &str) -> Node {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let store = Store::open(&home).unwrap();
        store.set_node_name(name).unwrap();
        Node {
            store,
            db: home.join(DB_FILE),
            _dir: dir,
        }
    }

    fn id(&self) -> String {
        self.store.node_id().to_string()
    }

    /// `agent` registered in a project directory named `shop`, holding `src/a.rs`.
    fn holder(&self, agent: &str) -> Hold {
        let shop = self._dir.path().join("work").join("shop");
        std::fs::create_dir_all(&shop).unwrap();
        let project = self
            .store
            .ensure_project("test", &shop.to_string_lossy())
            .unwrap();
        self.store
            .register_agent(RegisterAgent {
                name: agent.into(),
                program: Program::Other,
                project: Some(project.abs_path.clone()),
                model: None,
                description: None,
            })
            .unwrap();
        self.store
            .acquire_holds(AcquireHolds {
                agent: format!("{agent}@shop"),
                project: project.abs_path,
                paths: vec!["src/a.rs".into()],
                ttl_seconds: Some(600),
                exclusive: true,
                reason: None,
                repo_wide: false,
            })
            .unwrap()
            .granted
            .remove(0)
    }

    fn released(&self, hold_id: &str) -> bool {
        Connection::open(&self.db)
            .unwrap()
            .query_row(
                "SELECT released_at IS NOT NULL FROM holds WHERE hold_id = ?1",
                [hold_id],
                |r| r.get(0),
            )
            .unwrap()
    }
}

/// A hub and a spoke paired with every permission; the spoke has pushed its events.
fn fed() -> (Node, Node) {
    let hub = Node::new("hub");
    let alpha = Node::new("alpha");
    let code = hub
        .store
        .create_invite("https://hub.example", &Perm::ALL, 3600)
        .unwrap();
    let (invite, req) = alpha.store.prepare_join(&code).unwrap();
    let resp = hub.store.accept_pairing(&req).unwrap();
    alpha.store.complete_join(&invite, &resp).unwrap();
    (hub, alpha)
}

fn push(hub: &Node, spoke: &Node) {
    let (events, _) = spoke.store.outbound_events(0, 10_000).unwrap();
    let report = hub
        .store
        .ingest(&spoke.id(), &events, &[spoke.store.node_entry().unwrap()])
        .unwrap();
    assert!(report.rejected.is_empty(), "{:?}", report.rejected);
}

#[test]
fn the_dashboard_will_not_release_another_nodes_hold() {
    let (hub, alpha) = fed();
    let carols = alpha.holder("carol");
    push(&hub, &alpha);
    assert!(!hub.released(&carols.hold_id), "the hub has alpha's hold");

    let err = hub
        .store
        .release_hold_from_dashboard(&carols.hold_id)
        .unwrap_err();
    assert_eq!(err.code(), "forbidden", "{err}");
    assert!(err.to_string().contains("owning node"), "{err}");
    assert!(!hub.released(&carols.hold_id));
}

/// The hub's own hold on the same path in its own `shop` project does not conflict with
/// alpha's (holds are node-local) but is shown with alpha's hold as a remote warning.
#[test]
fn an_overlapping_hold_on_another_node_is_a_remote_warning() {
    let (hub, alpha) = fed();
    let carols = alpha.holder("carol");
    // Two of alpha's holds that must not warn: another path in `shop`, and the same path in
    // a project with another name.
    let shop = alpha.store.project("shop").unwrap().abs_path;
    let other = alpha._dir.path().join("work").join("other");
    std::fs::create_dir_all(&other).unwrap();
    let other = alpha
        .store
        .ensure_project("test", &other.to_string_lossy())
        .unwrap()
        .abs_path;
    for (project, path) in [(&shop, "docs/guide.md"), (&other, "src/a.rs")] {
        alpha
            .store
            .acquire_holds(AcquireHolds {
                agent: "carol@shop".into(),
                project: project.clone(),
                paths: vec![path.into()],
                ttl_seconds: Some(600),
                exclusive: true,
                reason: None,
                repo_wide: false,
            })
            .unwrap();
    }
    push(&hub, &alpha);
    let bobs = hub.holder("bob");

    let snap = hub.store.dashboard_snapshot(DashboardView::LOCAL).unwrap();
    let bob = snap
        .holds
        .iter()
        .find(|h| h.hold.hold_id == bobs.hold_id)
        .unwrap();
    assert!(bob.conflicts.is_empty(), "{:?}", bob.conflicts);
    assert_eq!(bob.remote_warnings.len(), 1, "{:?}", bob.remote_warnings);
    let warning = &bob.remote_warnings[0];
    assert_eq!(warning.hold_id, carols.hold_id);
    assert_eq!(warning.node.as_deref(), Some("alpha"));
    assert_eq!(warning.pattern, "src/a.rs");
}

/// A closed request is left alone, and an unknown one is not found.
#[test]
fn the_dashboard_will_not_expire_a_closed_or_unknown_request() {
    let node = Node::new("solo");
    for name in ["lead", "worker"] {
        node.store
            .register_agent(RegisterAgent {
                name: name.into(),
                program: Program::Other,
                project: None,
                model: None,
                description: None,
            })
            .unwrap();
    }
    let open = || {
        node.store
            .send_message(SendMessage {
                from: "lead".into(),
                to: vec!["worker".into()],
                subject: "task".into(),
                body: "b".into(),
                request: Some(NewRequest::default()),
                ..Default::default()
            })
            .unwrap()
            .request_id
            .unwrap()
    };
    let act = |id: &str, action: RequestAction| {
        node.store
            .update_request(RequestUpdate {
                agent: "worker".into(),
                request_id: id.into(),
                action,
                note: None,
                due_in_seconds: None,
            })
            .unwrap();
    };
    let done = open();
    act(&done, RequestAction::Complete);
    let cancelled = open();
    node.store
        .update_request(RequestUpdate {
            agent: "lead".into(),
            request_id: cancelled.clone(),
            action: RequestAction::Cancel,
            note: None,
            due_in_seconds: None,
        })
        .unwrap();

    for (id, status) in [(&done, "done"), (&cancelled, "cancelled")] {
        let err = node.store.expire_request_from_dashboard(id).unwrap_err();
        assert_eq!(err.code(), "conflict", "{err}");
        assert!(
            err.to_string().contains(&format!("already {status}")),
            "{err}"
        );
        assert_eq!(
            format!("{:?}", node.store.request(id).unwrap().status).to_lowercase(),
            status
        );
    }
    let err = node
        .store
        .expire_request_from_dashboard("01NOSUCHREQUEST")
        .unwrap_err();
    assert_eq!(err.code(), "not_found", "{err}");
}
