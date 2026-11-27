//! Shared by the relay tests: nodes in temp homes, a hub on a loopback listener, and a
//! relay on another loopback listener that forwards to the hub and edits what a rule says.
//! The relay is the attacker of the tests: something that answers at the address a spoke
//! dials, in front of the real hub.

#![allow(dead_code)]

#[path = "fed_hub.rs"]
mod fed_hub;
pub use fed_hub::*;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use another_core::*;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use serde_json::Value;

/// Change the address `spoke` has stored for its hub, as an edited invite would have set it.
pub fn point_at(spoke: &Node, url: &str) {
    let changed = spoke
        .db()
        .execute("UPDATE peers SET url = ?1 WHERE role = 'hub'", [url])
        .unwrap();
    assert_eq!(changed, 1, "the spoke has one hub row");
}

/// Everything about a spoke that a refused answer must leave as it was.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncState {
    pub push_cursor: i64,
    pub pull_cursor: i64,
    pub anchors: PullAnchors,
    /// Every `nodes` row: id, name, key.
    pub nodes: Vec<(String, String, Option<String>)>,
    pub last_sync_at: Option<i64>,
}

pub fn sync_state(spoke: &Node, hub_id: &str) -> SyncState {
    let (push_cursor, pull_cursor) = spoke.store.push_cursor(hub_id).unwrap();
    let conn = spoke.db();
    let nodes = conn
        .prepare("SELECT node_id, display_name, public_key FROM nodes ORDER BY node_id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let last_sync_at = conn
        .query_row(
            "SELECT last_sync_at FROM peers WHERE node_id = ?1",
            [hub_id],
            |r| r.get(0),
        )
        .unwrap();
    SyncState {
        push_cursor,
        pull_cursor,
        anchors: spoke.store.pull_anchors().unwrap(),
        nodes,
        last_sync_at,
    }
}

/// How many events authored by `author` the node holds.
pub fn events_from(holder: &Node, author: &str) -> i64 {
    holder
        .db()
        .query_row(
            "SELECT count(*) FROM events WHERE node_id = ?1",
            [author],
            |r| r.get(0),
        )
        .unwrap()
}

/// How many events of `kind` the node holds.
pub fn events_of_kind(holder: &Node, kind: &str) -> i64 {
    holder
        .db()
        .query_row("SELECT count(*) FROM events WHERE kind = ?1", [kind], |r| {
            r.get(0)
        })
        .unwrap()
}

/// The key the node has stored for `node_id`, or `None` when it has no row for it.
pub fn pinned_key(holder: &Node, node_id: &str) -> Option<Option<String>> {
    use rusqlite::OptionalExtension;
    holder
        .db()
        .query_row(
            "SELECT public_key FROM nodes WHERE node_id = ?1",
            [node_id],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
}

/// One request as the relay received it.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    /// The path with its query.
    pub path: String,
    /// Header names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn remove_header(&mut self, name: &str) {
        self.headers.retain(|(n, _)| n != name);
    }

    pub fn set_header(&mut self, name: &str, value: &str) {
        self.remove_header(name);
        self.headers.push((name.to_string(), value.to_string()));
    }

    pub fn is_push(&self) -> bool {
        self.path.starts_with("/fed/push")
    }

    pub fn is_pull(&self) -> bool {
        self.path.starts_with("/fed/pull")
    }
}

/// One answer as the relay gives it to the spoke.
#[derive(Debug, Clone)]
pub struct Answer {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Answer {
    pub fn json(status: u16, body: &Value) -> Answer {
        Answer {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.to_string().into_bytes(),
        }
    }

    /// Rewrite the JSON body and keep the headers, as a relay that edits an answer and
    /// passes on whatever the hub sent with it.
    pub fn edit(mut self, f: impl FnOnce(&mut Value)) -> Answer {
        let mut v: Value = serde_json::from_slice(&self.body).unwrap();
        f(&mut v);
        self.body = v.to_string().into_bytes();
        self
    }
}

/// Where the relay forwards to.
pub struct Upstream(String);

impl Upstream {
    /// Send `req` to the hub as it is and return the hub's answer with its headers.
    pub fn send(&self, req: &Seen) -> Answer {
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .proxy(None)
                .timeout_global(Some(Duration::from_secs(30)))
                .build(),
        );
        let url = format!("{}{}", self.0, req.path);
        let passed = |name: &str| name.starts_with("x-another-") || name == "content-type";
        let result = if req.method == "POST" {
            let mut r = agent.post(&url);
            for (n, v) in req.headers.iter().filter(|(n, _)| passed(n)) {
                r = r.header(n.as_str(), v.as_str());
            }
            r.send(&req.body[..])
        } else {
            let mut r = agent.get(&url);
            for (n, v) in req.headers.iter().filter(|(n, _)| passed(n)) {
                r = r.header(n.as_str(), v.as_str());
            }
            r.call()
        };
        let mut resp = result.unwrap();
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .filter(|(n, _)| {
                let n = n.as_str();
                n.starts_with("x-another-") || n == "content-type" || n == "upgrade"
            })
            .map(|(n, v)| (n.as_str().to_string(), v.to_str().unwrap().to_string()))
            .collect();
        let body = resp
            .body_mut()
            .with_config()
            .limit(64 * 1024 * 1024)
            .read_to_vec()
            .unwrap();
        Answer {
            status,
            headers,
            body,
        }
    }
}

type Rule = Arc<dyn Fn(Seen, &Upstream) -> Answer + Send + Sync>;

struct RelayState {
    upstream: Mutex<String>,
    rule: Mutex<Rule>,
    seen: Mutex<Vec<Seen>>,
}

pub struct Relay {
    state: Arc<RelayState>,
    served: Served,
}

impl Relay {
    /// A relay in front of the hub at `upstream`. Until told otherwise it forwards every
    /// request and returns every answer unchanged.
    pub fn start(upstream: &str) -> Relay {
        let honest: Rule = Arc::new(|req, hub| hub.send(&req));
        let state = Arc::new(RelayState {
            upstream: Mutex::new(upstream.to_string()),
            rule: Mutex::new(honest),
            seen: Mutex::new(Vec::new()),
        });
        let app = axum::Router::new()
            .fallback(relay)
            .layer(DefaultBodyLimit::disable())
            .with_state(state.clone());
        Relay {
            state,
            served: serve(app),
        }
    }

    pub fn url(&self) -> &str {
        &self.served.url
    }

    /// What the relay does with each request from now on.
    pub fn set(&self, rule: impl Fn(Seen, &Upstream) -> Answer + Send + Sync + 'static) {
        *self.state.rule.lock().unwrap() = Arc::new(rule);
    }

    pub fn honest(&self) {
        self.set(|req, hub| hub.send(&req));
    }

    /// Forward to another hub from now on.
    pub fn retarget(&self, upstream: &str) {
        *self.state.upstream.lock().unwrap() = upstream.to_string();
    }

    /// Every request received so far, oldest first.
    pub fn seen(&self) -> Vec<Seen> {
        self.state.seen.lock().unwrap().clone()
    }

    pub fn forget(&self) {
        self.state.seen.lock().unwrap().clear();
    }
}

async fn relay(
    State(state): State<Arc<RelayState>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let seen = Seen {
        method: method.to_string(),
        path: uri
            .path_and_query()
            .map(|p| p.as_str().to_string())
            .unwrap_or_default(),
        headers: headers
            .iter()
            .filter_map(|(n, v)| Some((n.as_str().to_string(), v.to_str().ok()?.to_string())))
            .collect(),
        body: body.to_vec(),
    };
    state.seen.lock().unwrap().push(seen.clone());
    let rule = state.rule.lock().unwrap().clone();
    let upstream = Upstream(state.upstream.lock().unwrap().clone());
    let answer = tokio::task::spawn_blocking(move || rule(seen, &upstream))
        .await
        .unwrap();
    let mut out = Response::builder().status(StatusCode::from_u16(answer.status).unwrap());
    for (n, v) in &answer.headers {
        out = out.header(n.as_str(), v.as_str());
    }
    out.body(axum::body::Body::from(answer.body)).unwrap()
}
