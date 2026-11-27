//! Shared federation hub test fixtures: temporary nodes, loopback HTTP serving in explicit
//! router modes, invite pairing, and audit helpers.
//!
//! Two router modes are kept distinct and named:
//! - [`HubMode::HttpFederation`]: the full [`another::serve::http_router`] with
//!   `federation: true` and `bound_port` set, matching `another serve --federation`
//!   (including `/health`, `Host` checks, and body limits).
//! - [`HubMode::FedRoutes`] / [`HubMode::FedRoutesWith`]: bare [`fed::router`] or
//!   [`fed::router_with`] (federation routes only, for relay tests and custom size limits).

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use another::fed;
use another::serve::{http_router, HttpOptions};
use another_core::*;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// Which router a test hub serves.
#[derive(Debug, Clone, Copy)]
pub enum HubMode {
    /// Full [`http_router`] with `federation: true` and `bound_port` set, as
    /// `another serve --federation` serves it.
    HttpFederation,
    /// Bare [`fed::router`] (federation routes only, with [`fed::Limits::CURRENT`]).
    FedRoutes,
    /// Bare [`fed::router_with`] (federation routes only, with caller-chosen [`fed::Limits`]).
    FedRoutesWith(fed::Limits),
}

/// A temporary node with its own SQLite database in a temporary directory.
///
/// Field order is significant for [`Drop`]: Rust drops struct fields in declaration order,
/// so `store` must precede `dir` to close the SQLite connection before [`tempfile::TempDir`]
/// deletes the directory (on Windows, deleting an open SQLite file fails silently in
/// `TempDir::drop`).
pub struct Node {
    pub store: Store,
    pub dir: tempfile::TempDir,
}

impl Node {
    /// Path to this node's SQLite database file.
    pub fn db_path(&self) -> PathBuf {
        self.dir.path().join(DB_FILE)
    }

    /// A second connection to this node's database, for reading rows and for edits a test
    /// makes as an operator with a SQL shell would.
    pub fn db(&self) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(self.db_path()).unwrap();
        conn.busy_timeout(Duration::from_secs(5)).unwrap();
        conn
    }

    pub fn id(&self) -> String {
        self.store.node_id().to_string()
    }
}

pub fn node(name: &str) -> Node {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.set_node_name(name).unwrap();
    Node { store, dir }
}

pub fn register(store: &Store, name: &str) {
    store
        .register_agent(RegisterAgent {
            name: name.into(),
            program: Program::Claude,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
}

/// A router served on a loopback port until stopped or dropped.
pub struct Served {
    pub url: String,
    pub port: u16,
    ct: CancellationToken,
    rt: Option<tokio::runtime::Runtime>,
}

impl Served {
    /// Stop serving and wait for the listener to close, returning the port it was bound to.
    pub fn stop(mut self) -> u16 {
        self.shutdown();
        self.port
    }

    fn shutdown(&mut self) {
        self.ct.cancel();
        if let Some(rt) = self.rt.take() {
            rt.shutdown_timeout(Duration::from_secs(5));
        }
    }
}

/// Dropping a `Served` cancels it and shuts its runtime down, blocking the dropping thread for
/// up to 5 seconds while in-flight requests finish.
impl Drop for Served {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Bind `127.0.0.1:<port>` (0 for any free port), build a router with the bound port and
/// shutdown token, and serve it until dropped or stopped.
pub fn serve_on(port: u16, build: impl FnOnce(u16, CancellationToken) -> axum::Router) -> Served {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let ct = CancellationToken::new();
    let listener = rt
        .block_on(tokio::net::TcpListener::bind(("127.0.0.1", port)))
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let app = build(addr.port(), ct.clone());
    let stop = ct.clone();
    rt.spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { stop.cancelled_owned().await })
        .await
        .unwrap();
    });
    Served {
        url: format!("http://{addr}"),
        port: addr.port(),
        ct,
        rt: Some(rt),
    }
}

/// Serve a pre-built router on an ephemeral loopback port.
pub fn serve(app: axum::Router) -> Served {
    serve_on(0, |_, _| app)
}

/// Build the router for `mode`.
pub fn router_for(
    store: Store,
    mode: HubMode,
    bound_port: u16,
    ct: CancellationToken,
) -> axum::Router {
    match mode {
        HubMode::HttpFederation => {
            let opts = HttpOptions {
                federation: true,
                bound_port,
                ..Default::default()
            };
            http_router(store, ct, &opts)
        }
        HubMode::FedRoutes => fed::router(store),
        HubMode::FedRoutesWith(limits) => fed::router_with(store, limits),
    }
}

/// Serve `store` in `mode` on an ephemeral loopback port.
pub fn serve_store(store: Store, mode: HubMode) -> Served {
    serve_store_on(store, mode, 0, |app| app)
}

/// Serve `store` in `mode` on `port` (0 for any free port), applying `wrap` around the router.
pub fn serve_store_on(
    store: Store,
    mode: HubMode,
    port: u16,
    wrap: impl FnOnce(axum::Router) -> axum::Router,
) -> Served {
    serve_on(port, move |bound_port, ct| {
        wrap(router_for(store, mode, bound_port, ct))
    })
}

/// A [`Node`] served over loopback HTTP.
///
/// Field order is significant for [`Drop`]: Rust drops struct fields in declaration order,
/// so `served` must precede `node` so the server shuts down its Tokio runtime and drops the
/// router's cloned [`Store`] before `node` closes its own [`Store`] and removes its
/// temporary directory.
pub struct Hub {
    pub served: Served,
    pub node: Node,
}

impl Hub {
    pub fn url(&self) -> &str {
        &self.served.url
    }

    pub fn port(&self) -> u16 {
        self.served.port
    }

    pub fn id(&self) -> String {
        self.node.id()
    }

    /// Stop serving and wait for the listener to close. The node and the port it was served
    /// on are handed back so it can be served again on the same address.
    pub fn stop(self) -> (Node, u16) {
        let port = self.served.stop();
        (self.node, port)
    }
}

/// Start a hub named `name` serving `mode`.
pub fn start_hub_in(name: &str, mode: HubMode) -> Hub {
    serve_node(node(name), mode, 0)
}

/// Serve an existing `node` in `mode` on `port` (0 for any free port).
pub fn serve_node(node: Node, mode: HubMode, port: u16) -> Hub {
    serve_node_layered(node, mode, port, |app| app)
}

/// Serve an existing `node` in `mode` on `port` (0 for any free port), applying `wrap` around
/// the router before serving.
pub fn serve_node_layered(
    node: Node,
    mode: HubMode,
    port: u16,
    wrap: impl FnOnce(axum::Router) -> axum::Router,
) -> Hub {
    let served = serve_store_on(node.store.clone(), mode, port, wrap);
    Hub { served, node }
}

/// A hub named `"hub"` served through [`http_router`] with `federation: true`.
pub fn start_http_hub() -> Hub {
    start_http_hub_named("hub")
}

/// A hub named `name` served through [`http_router`] with `federation: true`.
pub fn start_http_hub_named(name: &str) -> Hub {
    start_hub_in(name, HubMode::HttpFederation)
}

/// A hub named `"hub"` serving bare [`fed::router`] routes only.
pub fn start_fed_hub() -> Hub {
    start_fed_hub_named("hub")
}

/// A hub named `name` serving bare [`fed::router`] routes only.
pub fn start_fed_hub_named(name: &str) -> Hub {
    start_hub_in(name, HubMode::FedRoutes)
}

/// A hub named `"hub"` serving bare [`fed::router_with`] routes under `limits`.
pub fn start_fed_hub_with(limits: fed::Limits) -> Hub {
    start_hub_in("hub", HubMode::FedRoutesWith(limits))
}

/// Pair `spoke` with `hub` directly using `perms`, at the hub's real address.
pub fn join_with(hub: &Hub, spoke: &Node, perms: &[Perm]) -> Peer {
    let code = hub
        .node
        .store
        .create_invite(hub.url(), perms, 3600)
        .unwrap();
    fed::join(&spoke.store, &code).unwrap()
}

/// Pair `spoke` with `hub` directly with [`Perm::ALL`], at the hub's real address.
pub fn join(hub: &Hub, spoke: &Node) -> Peer {
    join_with(hub, spoke, &Perm::ALL)
}

pub fn decode_invite(code: &str) -> Value {
    let raw = code.strip_prefix("another-invite:").unwrap();
    serde_json::from_slice(&crypto::unb64(raw).unwrap()).unwrap()
}

pub fn encode_invite(invite: &Value) -> String {
    format!(
        "another-invite:{}",
        crypto::b64(invite.to_string().as_bytes())
    )
}

/// The audit rows of `store` for `operation`, oldest first.
pub fn audit_rows(store: &Store, operation: &str) -> Vec<AuditEntry> {
    let mut rows: Vec<AuditEntry> = store
        .audit_log(500)
        .unwrap()
        .into_iter()
        .filter(|a| a.operation == operation)
        .collect();
    rows.sort_by_key(|a| a.audit_id);
    rows
}
