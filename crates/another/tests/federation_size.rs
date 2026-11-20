//! Sync and size (Q26, D99): a hub and spokes over real loopback HTTP, each in its own temp
//! home. An honest sync never stalls on the size of a batch or a page, a body over a limit
//! is still refused, and a node on this build syncs with a node on a build before D99 in
//! both directions.

use std::io::Read;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use another::fed::{self, Limits};
use another::serve::{http_router, HttpOptions};
use another_core::*;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

struct Node {
    dir: tempfile::TempDir,
    store: Store,
}

impl Node {
    fn db(&self) -> PathBuf {
        self.dir.path().join(DB_FILE)
    }
}

fn node(name: &str) -> Node {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.set_node_name(name).unwrap();
    Node { dir, store }
}

fn register(store: &Store, name: &str) {
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

struct Hub {
    node: Node,
    url: String,
    port: u16,
    ct: CancellationToken,
    rt: tokio::runtime::Runtime,
}

impl Hub {
    /// Stop serving and wait for the listener to close. The node, and the port it was
    /// served on, are handed back so it can be served again.
    fn stop(self) -> (Node, u16) {
        self.ct.cancel();
        self.rt.shutdown_timeout(std::time::Duration::from_secs(5));
        (self.node, self.port)
    }
}

/// The bodies a recording hub saw on the wire: every `/fed/push` request and every
/// `/fed/pull` answer, byte for byte.
#[derive(Default)]
struct Recorded {
    pushes: Vec<Vec<u8>>,
    pulls: Vec<Vec<u8>>,
}

type Recording = std::sync::Arc<std::sync::Mutex<Recorded>>;

/// A layer in front of the hub's routes that keeps what passes through it, as a recording
/// proxy between the spoke and the hub would, and passes the same bytes on.
async fn record(
    axum::extract::State(recording): axum::extract::State<Recording>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let path = req.uri().path().to_string();
    let (parts, body) = req.into_parts();
    let sent = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    if path == "/fed/push" {
        recording.lock().unwrap().pushes.push(sent.to_vec());
    }
    let req = axum::extract::Request::from_parts(parts, axum::body::Body::from(sent));
    let resp = next.run(req).await;
    if path != "/fed/pull" {
        return resp;
    }
    let (parts, body) = resp.into_parts();
    let answered = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    recording.lock().unwrap().pulls.push(answered.to_vec());
    axum::response::Response::from_parts(parts, axum::body::Body::from(answered))
}

/// A hub on this build, served as `another serve --federation` serves it.
fn start_hub() -> Hub {
    start_hub_with(None)
}

/// With limits given, a hub that serves only the federation routes under them:
/// `Limits::BEFORE_D99` is a hub on a build before D99.
fn start_hub_with(limits: Option<Limits>) -> Hub {
    serve(node("hub"), limits, 0, None)
}

/// A hub on this build whose pushes and pull answers are recorded.
fn start_recording_hub() -> (Hub, Recording) {
    let recording = Recording::default();
    (
        serve(node("hub"), None, 0, Some(recording.clone())),
        recording,
    )
}

/// Serve `node` as a hub on `port` (0: any free port).
fn serve(node: Node, limits: Option<Limits>, port: u16, recording: Option<Recording>) -> Hub {
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
    let app = match limits {
        Some(limits) => fed::router_with(node.store.clone(), limits),
        None => {
            let opts = HttpOptions {
                federation: true,
                bound_port: addr.port(),
                ..Default::default()
            };
            http_router(node.store.clone(), ct.clone(), &opts)
        }
    };
    let app = match recording {
        Some(recording) => app.layer(axum::middleware::from_fn_with_state(recording, record)),
        None => app,
    };
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
    Hub {
        node,
        url: format!("http://{addr}"),
        port: addr.port(),
        ct,
        rt,
    }
}

fn join(hub: &Hub, spoke: &Node) {
    join_with(hub, spoke, &Perm::ALL);
}

fn join_with(hub: &Hub, spoke: &Node, perms: &[Perm]) {
    let code = hub.node.store.create_invite(&hub.url, perms, 3600).unwrap();
    fed::join(&spoke.store, &code).unwrap();
}

fn send(store: &Store, from: &str, to: &str, subject: &str, body: String) {
    store
        .send_message(SendMessage {
            from: from.into(),
            to: vec![to.into()],
            subject: subject.into(),
            body,
            ..Default::default()
        })
        .unwrap();
}

/// Subjects of the mail `agent` holds, sorted (an inbox orders by sending time, and
/// messages sent in one millisecond tie; arrival order is read from the event log).
fn inbox_subjects(store: &Store, agent: &str) -> Vec<String> {
    let mut subjects: Vec<String> = store
        .inbox(
            agent,
            InboxQuery {
                unread_only: false,
                urgent_only: false,
                limit: 5000,
            },
        )
        .unwrap()
        .messages
        .into_iter()
        .map(|m| m.message.subject)
        .collect();
    subjects.sort();
    subjects
}

fn count(db: &PathBuf, sql: &str) -> i64 {
    Connection::open(db)
        .unwrap()
        .query_row(sql, [], |r| r.get(0))
        .unwrap()
}

/// Requests the hub answered, by route: (`POST /fed/push`, `GET /fed/pull`).
fn round_trips(hub: &Hub) -> (i64, i64) {
    let of = |op: &str| {
        count(
            &hub.node.db(),
            &format!("SELECT count(*) FROM audit_log WHERE operation = '{op}' AND outcome = 'ok'"),
        )
    };
    (of("POST /fed/push"), of("GET /fed/pull"))
}

/// The events of `origin` in `n`'s log, in the order `n` stored them: a message by its
/// subject, anything else by its kind.
fn stored_from(n: &Node, origin: &str) -> Vec<String> {
    let c = Connection::open(n.db()).unwrap();
    let mut stmt = c
        .prepare("SELECT kind, payload FROM events WHERE node_id = ?1 ORDER BY seq")
        .unwrap();
    let rows = stmt
        .query_map([origin], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .unwrap();
    rows.map(|r| {
        let (kind, payload) = r.unwrap();
        let p: Value = serde_json::from_str(&payload).unwrap();
        match p.get("subject").and_then(Value::as_str) {
            Some(subject) if kind == "message.sent" => subject.to_string(),
            _ => kind,
        }
    })
    .collect()
}

fn http() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .proxy(None)
            .build(),
    )
}

const BODY: usize = 256 * 1024;
const MIB: usize = 1024 * 1024;

/// The budgets and the limits are related in one place: one event at the cap fits a batch
/// and a page, and each budget is under its limit. Raising a budget over its limit, or
/// cutting one under a single event, is caught here (and stops the build in
/// `another-core`).
#[test]
fn each_budget_fits_one_event_and_stays_under_its_limit() {
    let now = Limits::CURRENT;
    assert!(now.consistent());
    assert_eq!(MAX_EVENT_WIRE_BYTES, 2 * MIB);
    assert_eq!((now.push_budget, now.push_body), (4 * MIB, 8 * MIB));
    assert_eq!((now.pull_budget, now.pull_body), (8 * MIB, 32 * MIB));
    assert_eq!(fed::PAIR_BODY_LIMIT_BYTES, 2 * MIB);
    assert_eq!(another::serve::HTTP_BODY_LIMIT_BYTES, 2 * MIB);
    for broken in [
        Limits {
            push_budget: now.push_body + 1,
            ..now
        },
        Limits {
            pull_budget: now.pull_body + 1,
            ..now
        },
        Limits {
            push_budget: MAX_EVENT_WIRE_BYTES,
            ..now
        },
        Limits {
            pull_budget: MAX_EVENT_WIRE_BYTES,
            ..now
        },
        // A hub that took a push it could not relay to a spoke.
        Limits {
            push_body: now.pull_body,
            ..now
        },
        Limits::BEFORE_D99,
    ] {
        assert!(!broken.consistent(), "{broken:?}");
    }
}

/// Nine honest messages with the largest body the mail rule allows, waiting in one push
/// batch: the sync delivers them. On a build that bounds a batch by count only, every sync
/// fails with 413.
#[test]
fn nine_largest_messages_in_one_push_batch_sync() {
    let hub = start_hub();
    let a = node("spokea");
    join(&hub, &a);
    register(&hub.node.store, "bob");
    register(&a.store, "alice");
    fed::sync(&a.store).unwrap();
    for i in 0..9 {
        send(
            &a.store,
            "alice",
            "bob@@hub",
            &format!("big {i}"),
            "x".repeat(BODY),
        );
    }
    let first = fed::sync(&a.store).map(|r| r.pushed);
    let second = fed::sync(&a.store).map(|r| r.pushed);
    let got = inbox_subjects(&hub.node.store, "bob").len();
    hub.ct.cancel();
    assert!(
        first.is_ok(),
        "first sync: {:#}; second sync: {:?}; bob has {got} of 9",
        first.as_ref().unwrap_err(),
        second.as_ref().map_err(|e| format!("{e:#}")),
    );
    assert_eq!(got, 9);
}

/// Forty-five such messages from the hub to one spoke, waiting in one pull page: the sync
/// delivers them. On a build that bounds a page by count only, every sync fails at the
/// client's read limit.
#[test]
fn forty_five_largest_messages_in_one_pull_page_sync() {
    let hub = start_hub();
    let c = node("spokec");
    join(&hub, &c);
    register(&hub.node.store, "bob");
    register(&c.store, "carol");
    fed::sync(&c.store).unwrap();
    for i in 0..45 {
        send(
            &hub.node.store,
            "bob",
            "carol@@spokec",
            &format!("pull {i}"),
            "y".repeat(BODY),
        );
    }
    let first = fed::sync(&c.store).map(|r| r.applied);
    let second = fed::sync(&c.store).map(|r| r.applied);
    let got = inbox_subjects(&c.store, "carol").len();
    hub.ct.cancel();
    assert!(
        first.is_ok(),
        "first sync: {:#}; second sync: {:?}; carol has {got} of 45",
        first.as_ref().unwrap_err(),
        second.as_ref().map_err(|e| format!("{e:#}")),
    );
    assert_eq!(got, 45);
}

/// Make the newest local `agent.registered` on `n` carry a description of `bytes` bytes, as
/// a build before D99 could have written it, signed with the node's own key. Returns the
/// event id and its size on the wire.
fn grow_last_registration(n: &Node, bytes: usize) -> (String, usize) {
    let c = Connection::open(n.db()).unwrap();
    let (seq, event_id, node_id, ts, actor, payload, prev_id): (
        i64,
        String,
        String,
        i64,
        String,
        String,
        Option<String>,
    ) = c
        .query_row(
            "SELECT seq, event_id, node_id, ts, actor, payload, prev_id FROM events
             WHERE kind = 'agent.registered'
               AND node_id = (SELECT node_id FROM nodes WHERE is_local = 1)
             ORDER BY seq DESC LIMIT 1",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .unwrap();
    let mut p: Value = serde_json::from_str(&payload).unwrap();
    p["description"] = json!("d".repeat(bytes));
    let payload = p.to_string();
    let secret: String = c
        .query_row("SELECT secret_key FROM nodes WHERE is_local = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    let key = crypto::decode_secret(&secret).unwrap();
    let kind = "agent.registered".to_string();
    let sig = crypto::sign(
        &key,
        &crypto::event_message(&event_id, &node_id, ts, &actor, &kind, &payload),
    );
    c.execute(
        "UPDATE events SET payload = ?2, sig = ?3 WHERE event_id = ?1",
        params![event_id, payload, sig],
    )
    .unwrap();
    let size = WireEvent {
        seq,
        event_id: event_id.clone(),
        node_id,
        ts,
        actor,
        kind,
        payload,
        prev_id,
        sig,
    }
    .wire_bytes();
    (event_id, size)
}

/// An event of 3 MiB that an earlier build left in a hub's own log (this build refuses to
/// write one over 2 MiB). A build before D99 delivered it on a pull, and so does this one:
/// every spoke receives it and the mail before and after it.
#[test]
fn a_3_mib_event_in_a_hubs_log_reaches_every_spoke_with_the_mail_around_it() {
    let hub = start_hub();
    let a = node("spokea");
    let c = node("spokec");
    join(&hub, &a);
    join(&hub, &c);
    register(&hub.node.store, "bob");
    register(&a.store, "alice");
    register(&c.store, "carol");
    fed::sync(&a.store).unwrap();
    fed::sync(&c.store).unwrap();
    for to in ["alice@@spokea", "carol@@spokec"] {
        send(&hub.node.store, "bob", to, "before", "small".into());
    }
    register(&hub.node.store, "wordy");
    let (event_id, size) = grow_last_registration(&hub.node, 3 * MIB);
    for to in ["alice@@spokea", "carol@@spokec"] {
        send(&hub.node.store, "wordy", to, "from wordy", "small".into());
        send(&hub.node.store, "bob", to, "after", "small".into());
    }
    let reports = [fed::sync(&a.store), fed::sync(&c.store)];
    let inboxes = [
        inbox_subjects(&a.store, "alice"),
        inbox_subjects(&c.store, "carol"),
    ];
    let stored = [&a, &c].map(|n| {
        count(
            &n.db(),
            &format!("SELECT count(*) FROM events WHERE event_id = '{event_id}'"),
        )
    });
    let known = [&a, &c].map(|n| n.store.agent("wordy@@hub").is_ok());
    hub.ct.cancel();
    assert!(size > 3 * MIB);
    for report in reports {
        let report = report.map_err(|e| format!("{e:#}")).unwrap();
        assert!(
            report.pull_rejected.is_empty(),
            "{:?}",
            report.pull_rejected
        );
    }
    for inbox in inboxes {
        assert_eq!(inbox, ["after", "before", "from wordy"]);
    }
    assert_eq!(stored, [1, 1]);
    assert_eq!(known, [true, true]);
}

/// The same event in a spoke's log. A build before D99 could not push it (413 on every
/// sync). This build pushes it, the hub stores it and relays it to a second spoke, and the
/// mail around it, the wordy agent's own included, is delivered. It is run twice: with the
/// spoke on this build, and with the spoke bounded as a build before D99 was.
#[test]
fn a_3_mib_event_in_a_spokes_log_reaches_the_hub_and_a_second_spoke() {
    for spoke_limits in [Limits::CURRENT, Limits::BEFORE_D99] {
        let hub = start_hub();
        let a = node("spokea");
        let c = node("spokec");
        join(&hub, &a);
        join(&hub, &c);
        register(&hub.node.store, "bob");
        register(&a.store, "alice");
        register(&c.store, "carol");
        fed::sync_with(&a.store, &spoke_limits).unwrap();
        fed::sync(&c.store).unwrap();
        fed::sync_with(&a.store, &spoke_limits).unwrap();
        for to in ["bob@@hub", "carol@@spokec"] {
            send(&a.store, "alice", to, "before", "small".into());
        }
        register(&a.store, "wordy");
        let (event_id, size) = grow_last_registration(&a, 3 * MIB);
        for to in ["bob@@hub", "carol@@spokec"] {
            send(&a.store, "wordy", to, "from wordy", "small".into());
            send(&a.store, "alice", to, "after", "small".into());
        }
        let pushed = fed::sync_with(&a.store, &spoke_limits);
        let again = fed::sync_with(&a.store, &spoke_limits);
        let pulled = fed::sync(&c.store);
        let inboxes = [
            inbox_subjects(&hub.node.store, "bob"),
            inbox_subjects(&c.store, "carol"),
        ];
        let stored = [&hub.node, &c].map(|n| {
            count(
                &n.db(),
                &format!("SELECT count(*) FROM events WHERE event_id = '{event_id}'"),
            )
        });
        let known = [&hub.node.store, &c.store].map(|s| s.agent("wordy@@spokea").is_ok());
        hub.ct.cancel();
        assert!(size > 3 * MIB);
        let pushed = pushed.map_err(|e| format!("{e:#}")).unwrap();
        assert!(
            pushed.push_rejected.is_empty(),
            "{:?}",
            pushed.push_rejected
        );
        let again = again.map_err(|e| format!("{e:#}")).unwrap();
        assert_eq!((again.pushed, again.push_duplicates), (0, 0));
        let pulled = pulled.map_err(|e| format!("{e:#}")).unwrap();
        assert!(
            pulled.pull_rejected.is_empty(),
            "{:?}",
            pulled.pull_rejected
        );
        for inbox in inboxes {
            assert_eq!(inbox, ["after", "before", "from wordy"]);
        }
        assert_eq!(stored, [1, 1]);
        assert_eq!(known, [true, true]);
    }
}

/// Mixed pairings around the same 3 MiB event, with one side bounded as a build before D99
/// was. An older hub's own event reaches a spoke on this build, and a hub on this build's
/// own event reaches an older spoke, as both did before. A spoke on this build cannot push
/// its 3 MiB event to an older hub, which reads 2 MiB: as before nothing after it leaves,
/// and now the sync says which event and why.
#[test]
fn a_3_mib_event_with_one_side_on_a_build_before_d99() {
    // Older hub, new spoke: pull delivers, push of the spoke's own is refused by the hub.
    let hub = start_hub_with(Some(Limits::BEFORE_D99));
    let a = node("spokea");
    join(&hub, &a);
    register(&hub.node.store, "bob");
    register(&a.store, "alice");
    fed::sync(&a.store).unwrap();
    register(&hub.node.store, "wordy");
    grow_last_registration(&hub.node, 3 * MIB);
    send(
        &hub.node.store,
        "wordy",
        "alice@@spokea",
        "from the hub",
        "small".into(),
    );
    let pulled = fed::sync(&a.store).map(|r| r.pull_rejected.len());
    let alice = inbox_subjects(&a.store, "alice");
    send(&a.store, "alice", "bob@@hub", "before", "small".into());
    register(&a.store, "wordier");
    let (event_id, _) = grow_last_registration(&a, 3 * MIB);
    send(&a.store, "alice", "bob@@hub", "after", "small".into());
    let first = format!("{:#}", fed::sync(&a.store).unwrap_err());
    let second = format!("{:#}", fed::sync(&a.store).unwrap_err());
    let bob = inbox_subjects(&hub.node.store, "bob");
    let url = hub.url.clone();
    // An event this build can send and an older hub refuses is not skipped (D-next): no
    // marker is written and the push cursor stays before the event.
    let markers = count(
        &a.db(),
        "SELECT count(*) FROM events WHERE kind = 'sync.skipped'",
    );
    let skip_rows = count(
        &a.db(),
        "SELECT count(*) FROM audit_log WHERE operation = 'sync skip'",
    );
    let event_seq = count(
        &a.db(),
        &format!("SELECT seq FROM events WHERE event_id = '{event_id}'"),
    );
    let push_cursor = a.store.push_cursor(hub.node.store.node_id()).unwrap().0;
    hub.ct.cancel();
    assert_eq!((markers, skip_rows), (0, 0));
    assert!(push_cursor < event_seq, "{push_cursor} >= {event_seq}");
    assert_eq!(pulled.map_err(|e| format!("{e:#}")), Ok(0));
    assert_eq!(alice, ["from the hub"]);
    for message in [first, second] {
        assert!(
            message.starts_with(&format!(
                "the hub at {url} refused event {event_id} (agent.registered) as too large"
            )) || message.starts_with(&format!(
                "the connection to the hub at {url} failed while event {event_id} \
                 (agent.registered) was sent alone"
            )),
            "{message}"
        );
        assert!(message.contains("retrying will not help"), "{message}");
    }
    assert_eq!(bob, ["before"]);

    // New hub, older spoke: the hub's own 3 MiB event fits the 10 MiB the spoke reads.
    let hub = start_hub();
    let c = node("spokec");
    join(&hub, &c);
    register(&hub.node.store, "bob");
    register(&c.store, "carol");
    fed::sync_with(&c.store, &Limits::BEFORE_D99).unwrap();
    register(&hub.node.store, "wordy");
    grow_last_registration(&hub.node, 3 * MIB);
    send(
        &hub.node.store,
        "wordy",
        "carol@@spokec",
        "from the hub",
        "small".into(),
    );
    let pulled = fed::sync_with(&c.store, &Limits::BEFORE_D99).map(|r| r.pull_rejected.len());
    let carol = inbox_subjects(&c.store, "carol");
    hub.ct.cancel();
    assert_eq!(pulled.map_err(|e| format!("{e:#}")), Ok(0));
    assert_eq!(carol, ["from the hub"]);
}

// ---------------------------------------------------------------------------
// An event that does not fit the transport even alone is skipped, loudly (Q26b, D-next).

/// The payloads of the `sync.skipped` events of `origin` that `n` stores, oldest first.
fn markers(n: &Node, origin: &str) -> Vec<Value> {
    payloads(n, "sync.skipped", origin)
}

fn payloads(n: &Node, kind: &str, origin: &str) -> Vec<Value> {
    let c = Connection::open(n.db()).unwrap();
    let mut stmt = c
        .prepare("SELECT payload FROM events WHERE kind = ?1 AND node_id = ?2 ORDER BY seq")
        .unwrap();
    let rows = stmt
        .query_map([kind, origin], |r| r.get::<_, String>(0))
        .unwrap();
    rows.map(|p| serde_json::from_str(&p.unwrap()).unwrap())
        .collect()
}

fn id_of(n: &Node) -> String {
    n.store.node_id().to_string()
}

fn holds_event(n: &Node, event_id: &str) -> bool {
    count(
        &n.db(),
        &format!("SELECT count(*) FROM events WHERE event_id = '{event_id}'"),
    ) == 1
}

/// Audit rows of `operation` in `n`'s audit log.
fn audit_rows(n: &Node, operation: &str) -> i64 {
    count(
        &n.db(),
        &format!("SELECT count(*) FROM audit_log WHERE operation = '{operation}'"),
    )
}

/// The key names of a JSON object, sorted.
fn keys(v: &Value) -> Vec<String> {
    let mut keys: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    keys
}

/// Run the built binary on `n`'s home, with no terminal.
fn cli(n: &Node, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_another"))
        .args(args)
        .env("ANOTHER_HOME", n.dir.path())
        .env_remove("ANOTHER_AGENT")
        .env_remove("ANOTHER_ADMIT_KEY")
        .env_remove("ANOTHER_LOG")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn out(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Write a signed event of `kind` into `n`'s own log, as a changed build of that node
/// could. `id` is its event id.
fn plant(n: &Node, id: &str, kind: &str, payload: &Value) {
    let c = Connection::open(n.db()).unwrap();
    let secret: String = c
        .query_row("SELECT secret_key FROM nodes WHERE is_local = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    let key = crypto::decode_secret(&secret).unwrap();
    let node_id = id_of(n);
    let ts = n.store.now();
    let payload = payload.to_string();
    let sig = crypto::sign(
        &key,
        &crypto::event_message(id, &node_id, ts, "system", kind, &payload),
    );
    c.execute(
        "INSERT INTO events (event_id, node_id, ts, actor, kind, payload, prev_id, sig)
         VALUES (?1, ?2, ?3, 'system', ?4, ?5, NULL, ?6)",
        params![id, node_id, ts, kind, payload, sig],
    )
    .unwrap();
}

/// A marker payload as a holder writes it, for an event of `event_node`.
fn marker_for(event_id: &str, event_node: &str, direction: &str) -> Value {
    json!({
        "event_id": event_id,
        "event_node": event_node,
        "event_kind": "agent.registered",
        "event_actor": "wordy",
        "bytes": 9_437_500,
        "direction": direction,
        "about": "agent wordy",
        "route": "directory",
        "route_nodes": [],
    })
}

/// The six steps `another sync` prints for a skipped `agent.registered` of agent `wordy`,
/// each as its own line, as SPEC Q26b section 4.10 gives them.
const WORDY_STEPS: [&str; 6] = [
    "another agent register wordy --program <program> --model <model> --description \"<4096 bytes or fewer>\" (add --project <path> if it has one; a model left out is cleared)",
    "each hold it has (another holds --agent wordy lists them): another hold acquire --agent wordy -p <project> <the same paths>, with --shared, --repo-wide, --reason and --ttl as the hold has them (the same hold is sent again; without -p from another directory it is a different hold, and without --shared a shared hold becomes exclusive or is refused)",
    "each resource of this node it holds a lease on: another release <resource id or kind:name> --agent wordy, then at once another borrow <the same> --agent wordy (a borrow without the release only renews, and a renewal brings nothing to a node that never had the lease. Between the two commands the resource has no lease unless another agent holds one: another agent on this node that borrows it in that moment becomes its owner, and if the resource is exclusive this agent's borrow is then refused. If another agent already holds a lease, that agent becomes the owner at the release and this agent comes back as a borrower)",
    "each resource of another node it borrowed: another release <resource id> --agent wordy, then another borrow <resource id> --agent wordy (the id is in brackets in another resources; kind:name names only this node's resources. If the borrow is refused, the release has already happened: it is refused when the hub does not grant borrowing, when the resource has no live owner, or when it cannot be reached from this machine)",
    "mail it sent to other nodes before step 1 is not delivered there and no command delivers it: send it again",
    "another sync",
];

const WORDY_MISSING: &str = "what this registration changed: the agent's description and model. If it was the agent's first registration, other nodes do not know the agent at all: nobody there can address it; mail it sent them is stored and not delivered; they do not see its holds, so an agent there is not warned before it edits the same files; they do not see its leases, so a resource it owns shows no owner there and cannot be borrowed from another node, and a resource it borrowed on another node is not held for it there";

/// The block `another sync` prints for an own skipped registration of agent `wordy`.
fn own_block(event_id: &str, bytes: usize) -> String {
    let mut block = format!(
        "skipped {event_id} (agent.registered, {bytes} bytes, written by wordy): it is in this \
         node's log and is larger than a hub reads in one push (8388608 bytes), so it was not \
         sent; the events after it were sent\n  about: agent wordy\n  missing on other nodes: \
         {WORDY_MISSING}\n  to get it across:\n"
    );
    for step in WORDY_STEPS {
        block.push_str(&format!("    {step}\n"));
    }
    block.push_str(&format!("  then: another sync ack {event_id} --owner\n"));
    block
}

/// The block a spoke prints for a skipped registration of agent `wordy` in the log of its
/// hub, named `hub`, to be remedied on `place`.
fn hub_block(event_id: &str, bytes: usize, place: &str) -> String {
    let mut block = format!(
        "skipped {event_id} (agent.registered, {bytes} bytes, written by wordy): it is in node \
         hub's log and is larger than a spoke reads in one pull answer (33554432 bytes), so \
         node hub did not send it; the events after it were sent\n  about: agent wordy\n  \
         missing on this node: {WORDY_MISSING}\n  to get it across, on {place}:\n"
    );
    for step in WORDY_STEPS {
        block.push_str(&format!("    {step}\n"));
    }
    block.push_str(&format!(
        "  then, on node hub: another sync ack {event_id} --owner\n"
    ));
    block
}

/// R1, R2, R3: an event in a spoke's log that does not fit a push even alone (here 9 MiB,
/// over the 8 MiB a hub reads) is stepped past. One sync delivers everything before and
/// after it, writes one marker in the spoke's log and pushes that marker. The event stays
/// in the spoke's log and never reaches the hub. Section 7: nothing on the wire changes:
/// every push body is `{"events": [...]}` of events with the nine fields they always had.
#[test]
fn an_event_over_the_push_limit_is_stepped_past_and_everything_after_it_arrives() {
    let (hub, recording) = start_recording_hub();
    let a = node("spokea");
    join(&hub, &a);
    register(&hub.node.store, "bob");
    register(&a.store, "alice");
    fed::sync(&a.store).unwrap();
    send(&a.store, "alice", "bob@@hub", "before", "small".into());
    register(&a.store, "wordy");
    let (event_id, size) = grow_last_registration(&a, 9 * MIB);
    send(&a.store, "alice", "bob@@hub", "after", "small".into());
    recording.lock().unwrap().pushes.clear();

    let report = fed::sync(&a.store);
    let got = inbox_subjects(&hub.node.store, "bob");
    let a_id = id_of(&a);
    let own = markers(&a, &a_id);
    let on_hub = markers(&hub.node, &a_id);
    let wordy_on_hub = hub.node.store.agent("wordy@@spokea").is_ok();
    let pushes: Vec<Value> = recording
        .lock()
        .unwrap()
        .pushes
        .iter()
        .map(|body| serde_json::from_slice(body).unwrap())
        .collect();
    let again = fed::sync(&a.store).map(|r| (r.pushed, r.push_duplicates));
    hub.ct.cancel();

    assert!(size > 9 * MIB);
    let report = report.map_err(|e| format!("{e:#}")).unwrap();
    assert!(
        report.push_rejected.is_empty(),
        "{:?}",
        report.push_rejected
    );
    assert_eq!(got, ["after", "before"]);
    assert!(holds_event(&a, &event_id));
    assert!(!holds_event(&hub.node, &event_id));
    assert!(!wordy_on_hub);
    assert_eq!(own.len(), 1, "{own:?}");
    assert_eq!(own[0]["event_id"], json!(event_id));
    assert_eq!(own[0]["event_kind"], json!("agent.registered"));
    assert_eq!(own[0]["bytes"], json!(size));
    assert_eq!(own[0]["direction"], json!("push"));
    assert_eq!(on_hub, own, "the hub holds the marker after the same sync");
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].event_id, event_id);
    assert_eq!(audit_rows(&a, "sync skip"), 1);
    assert_eq!(again.map_err(|e| format!("{e:#}")), Ok((0, 0)));

    assert!(!pushes.is_empty());
    let mut pushed_kinds = Vec::new();
    for body in &pushes {
        assert_eq!(keys(body), ["events"]);
        for event in body["events"].as_array().unwrap() {
            assert_eq!(
                keys(event),
                [
                    "actor", "event_id", "kind", "node_id", "payload", "prev_id", "seq", "sig",
                    "ts"
                ]
            );
            assert_ne!(event["event_id"], json!(event_id));
            pushed_kinds.push(event["kind"].as_str().unwrap().to_string());
        }
    }
    assert!(pushed_kinds.contains(&"sync.skipped".to_string()));
}

/// R3: when the oversized event is the last in the log, the batch is empty and no request
/// carries it; the cursor moves, the marker is written, and the same sync pushes it.
#[test]
fn an_oversized_event_last_in_the_log_is_marked_and_the_marker_pushed_in_one_sync() {
    let hub = start_hub();
    let a = node("spokea");
    join(&hub, &a);
    register(&a.store, "alice");
    fed::sync(&a.store).unwrap();
    register(&a.store, "wordy");
    let (event_id, _) = grow_last_registration(&a, 9 * MIB);

    let report = fed::sync(&a.store).map(|r| r.push_rejected.len());
    let a_id = id_of(&a);
    let own = markers(&a, &a_id);
    let on_hub = markers(&hub.node, &a_id);
    let event_on_hub = holds_event(&hub.node, &event_id);
    hub.ct.cancel();

    assert_eq!(report.map_err(|e| format!("{e:#}")), Ok(0));
    assert_eq!(own.len(), 1);
    assert_eq!(own[0]["event_id"], json!(event_id));
    assert_eq!(on_hub, own);
    assert!(!event_on_hub);
}

/// R1: two oversized events in a row are both stepped past and marked in one sync, and
/// everything after both is delivered.
#[test]
fn two_oversized_events_in_a_row_are_both_stepped_past_and_marked() {
    let hub = start_hub();
    let a = node("spokea");
    join(&hub, &a);
    register(&hub.node.store, "bob");
    register(&a.store, "alice");
    fed::sync(&a.store).unwrap();
    send(&a.store, "alice", "bob@@hub", "before", "small".into());
    register(&a.store, "wordy");
    let (first, _) = grow_last_registration(&a, 9 * MIB);
    register(&a.store, "wordier");
    let (second, _) = grow_last_registration(&a, 9 * MIB);
    send(&a.store, "alice", "bob@@hub", "after", "small".into());

    let report = fed::sync(&a.store).map(|r| r.skipped.len());
    let a_id = id_of(&a);
    let named: Vec<Value> = markers(&a, &a_id)
        .iter()
        .map(|m| m["event_id"].clone())
        .collect();
    let on_hub = markers(&hub.node, &a_id).len();
    let got = inbox_subjects(&hub.node.store, "bob");
    let rows = audit_rows(&a, "sync skip");
    hub.ct.cancel();

    assert_eq!(report.map_err(|e| format!("{e:#}")), Ok(2));
    assert_eq!(named, [json!(first), json!(second)]);
    assert_eq!(on_hub, 2);
    assert_eq!(rows, 2);
    assert_eq!(got, ["after", "before"]);
}

/// A hub with agent `bob` and a spoke `spokec` with agent `carol`, synced, then in the
/// hub's own log: mail `before`, a 33 MiB registration of `wordy` (over the 32 MiB a spoke
/// reads) and mail `after`. Returns the oversized event's id and size.
fn hub_with_an_oversized_event(hub: &Hub, c: &Node) -> (String, usize) {
    join(hub, c);
    register(&hub.node.store, "bob");
    register(&c.store, "carol");
    fed::sync(&c.store).unwrap();
    send(
        &hub.node.store,
        "bob",
        "carol@@spokec",
        "before",
        "small".into(),
    );
    register(&hub.node.store, "wordy");
    let grown = grow_last_registration(&hub.node, 33 * MIB);
    send(
        &hub.node.store,
        "bob",
        "carol@@spokec",
        "after",
        "small".into(),
    );
    grown
}

/// A hub with agent `bob` and a spoke `spokea` with agent `alice`, synced, then in the
/// spoke's log: mail `before`, a 9 MiB registration of `wordy` (over the 8 MiB a hub
/// reads) and mail `after`. Returns the oversized event's id and size.
fn spoke_with_an_oversized_event(hub: &Hub, a: &Node) -> (String, usize) {
    join(hub, a);
    register(&hub.node.store, "bob");
    register(&a.store, "alice");
    fed::sync(&a.store).unwrap();
    send(&a.store, "alice", "bob@@hub", "before", "small".into());
    register(&a.store, "wordy");
    let grown = grow_last_registration(a, 9 * MIB);
    send(&a.store, "alice", "bob@@hub", "after", "small".into());
    grown
}

/// R4, R5: the same on the hub's side. An event in the hub's own log that does not fit a
/// pull answer even alone (here 33 MiB, over the 32 MiB a spoke reads) is stepped past.
/// One sync of a spoke delivers everything before and after it; the hub writes one marker
/// and one audit row before it answers, and the spoke holds the marker after the same
/// sync, because the page that stepped past the event said there was more. Section 7:
/// every pull answer has the five fields it always had.
#[test]
fn an_event_over_the_pull_limit_is_stepped_past_and_everything_after_it_arrives() {
    let (hub, recording) = start_recording_hub();
    let c = node("spokec");
    let (event_id, size) = hub_with_an_oversized_event(&hub, &c);
    recording.lock().unwrap().pulls.clear();

    let report = fed::sync(&c.store);
    let got = inbox_subjects(&c.store, "carol");
    let hub_id = id_of(&hub.node);
    let on_hub = markers(&hub.node, &hub_id);
    let on_spoke = markers(&c, &hub_id);
    let rows = audit_rows(&hub.node, "sync skip");
    let pulls: Vec<Value> = recording
        .lock()
        .unwrap()
        .pulls
        .iter()
        .map(|body| serde_json::from_slice(body).unwrap())
        .collect();
    let again = fed::sync(&c.store).map(|r| (r.pulled, r.skipped.len()));
    let markers_after = markers(&hub.node, &hub_id).len();
    hub.ct.cancel();

    assert!(size > 33 * MIB);
    let report = report.map_err(|e| format!("{e:#}")).unwrap();
    assert!(
        report.pull_rejected.is_empty(),
        "{:?}",
        report.pull_rejected
    );
    assert_eq!(got, ["after", "before"]);
    assert!(!holds_event(&c, &event_id));
    assert_eq!(on_hub.len(), 1, "{on_hub:?}");
    assert_eq!(on_hub[0]["event_id"], json!(event_id));
    assert_eq!(on_hub[0]["bytes"], json!(size));
    assert_eq!(on_hub[0]["direction"], json!("pull"));
    assert_eq!(rows, 1);
    assert_eq!(
        on_spoke, on_hub,
        "the spoke holds the marker after the same sync"
    );
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].holder, hub_id);
    assert_eq!(again.map_err(|e| format!("{e:#}")), Ok((0, 1)));
    assert_eq!(markers_after, 1);

    assert!(pulls.len() >= 2, "{}", pulls.len());
    assert_eq!(pulls[0]["more"], json!(true));
    for body in &pulls {
        assert_eq!(keys(body), ["cursor", "events", "more", "nodes", "reset"]);
        for event in body["events"].as_array().unwrap() {
            assert_ne!(event["event_id"], json!(event_id));
            assert_eq!(keys(event).len(), 9);
        }
    }
}

/// R2: a push that fails moves no cursor and writes no marker; the next sync that reaches
/// the hub writes one marker and one audit row.
#[test]
fn a_push_that_fails_leaves_no_marker_and_no_moved_cursor() {
    let hub = start_hub();
    let a = node("spokea");
    spoke_with_an_oversized_event(&hub, &a);
    let hub_id = id_of(&hub.node);
    let a_id = id_of(&a);
    let cursor = a.store.push_cursor(&hub_id).unwrap().0;
    let (hub_node, port) = hub.stop();

    let failed = fed::sync(&a.store).map(|r| r.pushed);
    let cursor_after = a.store.push_cursor(&hub_id).unwrap().0;
    let left = (markers(&a, &a_id).len(), audit_rows(&a, "sync skip"));

    let hub = serve(hub_node, None, port, None);
    let back = fed::sync(&a.store).map(|r| r.push_rejected.len());
    let then = (markers(&a, &a_id).len(), audit_rows(&a, "sync skip"));
    let got = inbox_subjects(&hub.node.store, "bob");
    hub.ct.cancel();

    assert!(failed.is_err(), "{failed:?}");
    assert_eq!(cursor_after, cursor);
    assert_eq!(left, (0, 0));
    assert_eq!(back.map_err(|e| format!("{e:#}")), Ok(0));
    assert_eq!(then, (1, 1));
    assert_eq!(got, ["after", "before"]);
}

/// R4, the edge of what a pull carries: an event whose answer, with the node directory, is
/// exactly what a spoke reads is sent; one byte more is skipped and marked.
#[test]
fn an_event_one_byte_over_the_pull_limit_is_skipped_and_one_at_it_is_sent() {
    let hub = start_hub();
    let c = node("spokec");
    join(&hub, &c);
    register(&c.store, "carol");
    fed::sync(&c.store).unwrap();
    let peer = id_of(&c);
    let hub_id = id_of(&hub.node);
    // The answer with no events, as the hub counts it: the widest cursor, less the comma
    // an event brings.
    let nodes = hub
        .node
        .store
        .pull_page(&peer, 0, &PullAnchors::default(), 1, usize::MAX)
        .unwrap()
        .0
        .nodes;
    let envelope = serde_json::to_vec(&PullResponse {
        events: Vec::new(),
        cursor: i64::MAX,
        more: false,
        nodes,
        reset: false,
    })
    .unwrap()
    .len()
        - 1;

    register(&hub.node.store, "wordy");
    let (_, base) = grow_last_registration(&hub.node, 0);
    let (at_id, at_size) =
        grow_last_registration(&hub.node, PULL_BODY_LIMIT_BYTES - envelope - 1 - base);
    assert_eq!(envelope + at_size + 1, PULL_BODY_LIMIT_BYTES);
    let at = fed::sync(&c.store).map(|r| r.skipped.len());
    let at_arrived = holds_event(&c, &at_id);
    let markers_at = markers(&hub.node, &hub_id).len();

    register(&hub.node.store, "wordz");
    let (_, base) = grow_last_registration(&hub.node, 0);
    let (over_id, over_size) =
        grow_last_registration(&hub.node, PULL_BODY_LIMIT_BYTES - envelope - base);
    assert_eq!(envelope + over_size + 1, PULL_BODY_LIMIT_BYTES + 1);
    register(&hub.node.store, "last");
    let over = fed::sync(&c.store).map(|r| r.skipped.len());
    let over_arrived = holds_event(&c, &over_id);
    let named: Vec<Value> = markers(&hub.node, &hub_id)
        .iter()
        .map(|m| m["event_id"].clone())
        .collect();
    let last_known = c.store.agent("last@@hub").is_ok();
    hub.ct.cancel();

    assert_eq!(at.map_err(|e| format!("{e:#}")), Ok(0));
    assert!(at_arrived);
    assert_eq!(markers_at, 0);
    assert_eq!(over.map_err(|e| format!("{e:#}")), Ok(1));
    assert!(!over_arrived);
    assert_eq!(named, [json!(over_id)]);
    assert!(last_known);
}

/// 4.3: a marker names the event and never carries its large field.
#[test]
fn a_marker_names_the_event_and_never_carries_its_large_field() {
    let hub = start_hub();
    let a = node("spokea");
    let (event_id, size) = spoke_with_an_oversized_event(&hub, &a);
    fed::sync(&a.store).unwrap();
    hub.ct.cancel();
    let a_id = id_of(&a);
    let raw: String = Connection::open(a.db())
        .unwrap()
        .query_row(
            "SELECT payload FROM events WHERE kind = 'sync.skipped'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(raw.len() < 2048, "{}", raw.len());
    assert!(!raw.contains(&"d".repeat(32)));
    let p: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        p,
        json!({
            "event_id": event_id,
            "event_node": a_id,
            "event_kind": "agent.registered",
            "event_actor": "wordy",
            "bytes": size,
            "direction": "push",
            "about": "agent wordy",
            "route": "directory",
            "route_nodes": [],
        })
    );
    assert_eq!(keys(&p).len(), 9);
    let (actor,): (String,) = Connection::open(a.db())
        .unwrap()
        .query_row(
            "SELECT actor FROM events WHERE kind = 'sync.skipped'",
            [],
            |r| Ok((r.get(0)?,)),
        )
        .unwrap();
    assert_eq!(actor, "system");
}

/// 4.3, R5: one marker per holder and skipped event, however often the event is stepped
/// past. On a spoke: three syncs, then a rescan from zero. On a hub: two spokes pulling at
/// the same moment.
#[test]
fn stepping_past_the_same_event_again_writes_no_second_marker() {
    let hub = start_hub();
    let a = node("spokea");
    spoke_with_an_oversized_event(&hub, &a);
    let hub_id = id_of(&hub.node);
    let a_id = id_of(&a);
    for _ in 0..3 {
        fed::sync(&a.store).unwrap();
    }
    a.store.reset_sync_cursors(&hub_id).unwrap();
    let rescanned = fed::sync(&a.store).map(|r| r.push_rejected.len());
    let on_spoke = (markers(&a, &a_id).len(), audit_rows(&a, "sync skip"));
    let relayed = markers(&hub.node, &a_id).len();
    hub.ct.cancel();
    assert_eq!(rescanned.map_err(|e| format!("{e:#}")), Ok(0));
    assert_eq!(on_spoke, (1, 1));
    assert_eq!(relayed, 1);

    let hub = start_hub();
    let c = node("spokec");
    let d = node("spoked");
    hub_with_an_oversized_event(&hub, &c);
    join(&hub, &d);
    let hub_id = id_of(&hub.node);
    let (first, second) = std::thread::scope(|s| {
        let one = s.spawn(|| fed::sync(&c.store).map(|r| r.skipped.len()));
        let two = s.spawn(|| fed::sync(&d.store).map(|r| r.skipped.len()));
        (one.join().unwrap(), two.join().unwrap())
    });
    let on_hub = (
        markers(&hub.node, &hub_id).len(),
        audit_rows(&hub.node, "sync skip"),
    );
    let seen = [&c, &d].map(|n| markers(n, &hub_id).len());
    hub.ct.cancel();
    assert_eq!(first.map_err(|e| format!("{e:#}")), Ok(1));
    assert_eq!(second.map_err(|e| format!("{e:#}")), Ok(1));
    assert_eq!(on_hub, (1, 1));
    assert_eq!(seen, [1, 1]);
}

/// R7: after a hub is restored from an older copy the spoke rescans from zero, steps past
/// the same event, finds its marker and writes nothing new; the marker is pushed again
/// and the hub holds it once.
#[test]
fn a_hub_reset_does_not_report_a_skip_twice() {
    let hub = start_hub();
    let a = node("spokea");
    join(&hub, &a);
    register(&hub.node.store, "bob");
    register(&a.store, "alice");
    fed::sync(&a.store).unwrap();
    let backup = hub.node.dir.path().join("backup.db");
    Connection::open(hub.node.db())
        .unwrap()
        .execute("VACUUM INTO ?1", [backup.to_string_lossy()])
        .unwrap();
    send(&a.store, "alice", "bob@@hub", "before", "small".into());
    register(&a.store, "wordy");
    let (event_id, _) = grow_last_registration(&a, 9 * MIB);
    send(&a.store, "alice", "bob@@hub", "after", "small".into());
    fed::sync(&a.store).unwrap();
    let a_id = id_of(&a);
    assert_eq!(markers(&hub.node, &a_id).len(), 1);

    // The operator restores the hub from the copy: every connection is closed first.
    let (Node { dir, store }, port) = hub.stop();
    drop(store);
    for suffix in ["", "-wal", "-shm"] {
        match std::fs::remove_file(dir.path().join(format!("{DB_FILE}{suffix}"))) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => panic!("cannot remove {DB_FILE}{suffix}: {e}"),
        }
    }
    std::fs::copy(&backup, dir.path().join(DB_FILE)).unwrap();
    let store = Store::open(dir.path()).unwrap();
    let hub = serve(Node { dir, store }, None, port, None);
    assert_eq!(markers(&hub.node, &a_id).len(), 0, "restored");

    let resynced = fed::sync(&a.store).map(|r| (r.hub_reset, r.skipped.len()));
    let on_spoke = (markers(&a, &a_id).len(), audit_rows(&a, "sync skip"));
    let on_hub = markers(&hub.node, &a_id);
    let got = inbox_subjects(&hub.node.store, "bob");
    let event_on_hub = holds_event(&hub.node, &event_id);
    hub.ct.cancel();

    assert_eq!(resynced.map_err(|e| format!("{e:#}")), Ok((true, 1)));
    assert_eq!(on_spoke, (1, 1));
    assert_eq!(on_hub.len(), 1);
    assert_eq!(on_hub[0]["event_id"], json!(event_id));
    assert_eq!(got, ["after", "before"]);
    assert!(!event_on_hub);
}

/// A spoke that pairs after the skips, and pulls from zero, holds both markers (one for a
/// spoke's event, one for the hub's own) and neither event, and everything else arrives
/// with no rejection.
#[test]
fn a_spoke_that_pairs_later_pulls_the_marker_and_not_the_event() {
    let hub = start_hub();
    let a = node("spokea");
    let (pushed_id, _) = spoke_with_an_oversized_event(&hub, &a);
    register(&hub.node.store, "huge");
    let (pulled_id, _) = grow_last_registration(&hub.node, 33 * MIB);
    register(&hub.node.store, "last");
    fed::sync(&a.store).unwrap();
    let hub_id = id_of(&hub.node);
    let a_id = id_of(&a);
    assert_eq!(markers(&hub.node, &hub_id).len(), 1);

    let late = node("late");
    join(&hub, &late);
    let report = fed::sync(&late.store);
    let from_spoke = markers(&late, &a_id);
    let from_hub = markers(&late, &hub_id);
    let events = [&pushed_id, &pulled_id].map(|id| holds_event(&late, id));
    let known =
        ["alice@@spokea", "bob@@hub", "last@@hub"].map(|agent| late.store.agent(agent).is_ok());
    let hub_markers = markers(&hub.node, &hub_id).len();
    hub.ct.cancel();

    let report = report.map_err(|e| format!("{e:#}")).unwrap();
    assert!(
        report.pull_rejected.is_empty(),
        "{:?}",
        report.pull_rejected
    );
    assert_eq!(from_spoke.len(), 1);
    assert_eq!(from_spoke[0]["event_id"], json!(pushed_id));
    assert_eq!(from_hub.len(), 1);
    assert_eq!(from_hub[0]["event_id"], json!(pulled_id));
    assert_eq!(events, [false, false]);
    assert_eq!(known, [true, true, true]);
    assert_eq!(hub_markers, 1);
    // It reports the hub's skip and not the other spoke's.
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].event_id, pulled_id);
}

/// 4.6: a spoke's marker for a directory event reaches a spoke without `read_feed`, as the
/// event would have. There it is a feed line only: `another sync` and `another needs`
/// repeat only this node's skips and its hub's.
#[test]
fn a_directory_marker_from_a_spoke_reaches_a_spoke_without_read_feed_as_a_feed_line_only() {
    let hub = start_hub();
    let a = node("spokea");
    let (event_id, size) = spoke_with_an_oversized_event(&hub, &a);
    fed::sync(&a.store).unwrap();
    let plain = node("plain");
    join_with(&hub, &plain, &[Perm::SendMail]);
    let synced = cli(&plain, &["sync"]);
    let a_id = id_of(&a);
    let stored = markers(&plain, &a_id).len();
    let feed = cli(&plain, &["activity", "--kinds", "sync"]);
    let needs = cli(&plain, &["--json", "needs"]);
    hub.ct.cancel();

    assert!(synced.status.success(), "{}", err(&synced));
    assert_eq!(stored, 1);
    assert!(!out(&synced).contains("skipped"), "{}", out(&synced));
    assert!(
        out(&feed).contains(&format!(
            "[spokea] skipped event {event_id} (agent.registered, {size} bytes, written by \
             wordy): too large to send; agent wordy"
        )),
        "{}",
        out(&feed)
    );
    let needs: Value = serde_json::from_str(&out(&needs)).unwrap();
    assert_eq!(needs["counts"]["skipped_event"], json!(0));
}

/// Section 6: a spoke's markers are repeated by no other node. Fifty markers from one
/// spoke give the hub and another spoke fifty feed lines and no report.
#[test]
fn a_spokes_markers_are_not_repeated_by_other_nodes() {
    let hub = start_hub();
    let m = node("mallory");
    let b = node("spokeb");
    join(&hub, &m);
    join(&hub, &b);
    let m_id = id_of(&m);
    for i in 0..50 {
        plant(
            &m,
            &format!("01PLANTED{i:02}"),
            "sync.skipped",
            &marker_for(&format!("01NOSUCH{i:02}"), &m_id, "push"),
        );
    }
    let pushed = fed::sync(&m.store).map(|r| r.push_rejected.len());
    let synced = cli(&b, &["sync"]);
    let stored = [&hub.node, &b].map(|n| markers(n, &m_id).len());
    let needs = [&hub.node, &b].map(|n| {
        let v: Value = serde_json::from_str(&out(&cli(n, &["--json", "needs"]))).unwrap();
        v["counts"]["skipped_event"].clone()
    });
    let feed = cli(&b, &["activity", "--kinds", "sync", "--limit", "200"]);
    hub.ct.cancel();

    assert_eq!(pushed.map_err(|e| format!("{e:#}")), Ok(0));
    assert!(synced.status.success(), "{}", err(&synced));
    assert_eq!(stored, [50, 50]);
    assert!(!out(&synced).contains("skipped"), "{}", out(&synced));
    assert_eq!(needs, [json!(0), json!(0)]);
    let lines = out(&feed);
    assert_eq!(
        lines
            .lines()
            .filter(|l| l.contains("[mallory] skipped event 01NOSUCH"))
            .count(),
        50,
        "{lines}"
    );
}

/// Section 6: a hub's marker for an event this node holds is not reported, so a hub cannot
/// make a spoke report as missing an event the spoke has.
#[test]
fn a_hubs_marker_for_an_event_this_node_holds_is_not_reported() {
    let hub = start_hub();
    let c = node("spokec");
    join(&hub, &c);
    register(&hub.node.store, "bob");
    fed::sync(&c.store).unwrap();
    let hub_id = id_of(&hub.node);
    let held: String = Connection::open(c.db())
        .unwrap()
        .query_row(
            "SELECT event_id FROM events WHERE kind = 'agent.registered' AND node_id = ?1",
            [&hub_id],
            |r| r.get(0),
        )
        .unwrap();
    plant(
        &hub.node,
        "01PLANTEDHELD",
        "sync.skipped",
        &marker_for(&held, &hub_id, "pull"),
    );
    let synced = cli(&c, &["sync"]);
    let stored = markers(&c, &hub_id).len();
    let reported = c.store.unacknowledged_skips().unwrap().len();
    // The same from the hub for an event this node does not hold is reported.
    plant(
        &hub.node,
        "01PLANTEDMISSING",
        "sync.skipped",
        &marker_for("01N0THE1D00000000000000000", &hub_id, "pull"),
    );
    let again = cli(&c, &["sync"]);
    hub.ct.cancel();

    assert!(synced.status.success(), "{}", err(&synced));
    assert_eq!(stored, 1);
    assert_eq!(reported, 0);
    assert!(!out(&synced).contains("skipped"), "{}", out(&synced));
    assert!(
        out(&again)
            .contains("\nskipped 01N0THE1D00000000000000000 (agent.registered, 9437500 bytes, "),
        "{}",
        out(&again)
    );
    assert!(!out(&again).contains(&held), "{}", out(&again));
}

/// 4.8: `another sync` prints the block for an unacknowledged own skip after every sync and
/// exits 0; `--json` carries it in `skipped` and its other keys are unchanged.
#[test]
fn another_sync_reports_an_unacknowledged_skip_every_time_and_exits_zero() {
    let hub = start_hub();
    let a = node("spokea");
    let (event_id, size) = spoke_with_an_oversized_event(&hub, &a);
    let runs = [0, 1, 2].map(|_| cli(&a, &["sync"]));
    let as_json = cli(&a, &["--json", "sync"]);
    let a_id = id_of(&a);
    let hub_id = id_of(&hub.node);
    let url = hub.url.clone();
    hub.ct.cancel();

    let want = own_block(&event_id, size);
    for run in &runs {
        assert_eq!(run.status.code(), Some(0), "{}", err(run));
        let text = out(run);
        let (first, rest) = text.split_once('\n').unwrap();
        assert!(
            first.starts_with(&format!("synced with {url}: pushed ")),
            "{first}"
        );
        assert_eq!(rest, want);
    }
    assert_eq!(as_json.status.code(), Some(0));
    let v: Value = serde_json::from_str(&out(&as_json)).unwrap();
    assert_eq!(
        keys(&v),
        [
            "applied",
            "hub",
            "hub_reset",
            "pull_duplicates",
            "pull_rejected",
            "pulled",
            "push_duplicates",
            "push_rejected",
            "pushed",
            "skipped",
            "skipped_total"
        ]
    );
    assert_eq!(v["skipped_total"], json!(1));
    let skipped = v["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 1);
    let s = &skipped[0];
    assert_eq!(
        keys(s),
        [
            "about",
            "ack",
            "actor",
            "bytes",
            "direction",
            "event_id",
            "event_node",
            "event_node_name",
            "holder",
            "holder_name",
            "kind",
            "marker_id",
            "missing",
            "remedy",
            "since"
        ]
    );
    assert_eq!(s["event_id"], json!(event_id));
    assert_eq!(s["event_node"], json!(a_id));
    assert_eq!(s["event_node_name"], json!("spokea"));
    assert_eq!(s["holder"], json!(a_id));
    assert_eq!(s["holder_name"], json!("spokea"));
    assert_ne!(s["holder"], json!(hub_id));
    assert_eq!(s["kind"], json!("agent.registered"));
    assert_eq!(s["bytes"], json!(size));
    assert_eq!(s["actor"], json!("wordy"));
    assert_eq!(s["about"], json!("agent wordy"));
    assert_eq!(s["direction"], json!("push"));
    assert_eq!(s["missing"], json!(WORDY_MISSING));
    assert_eq!(s["remedy"], json!(WORDY_STEPS));
    assert_eq!(
        s["ack"],
        json!(format!("another sync ack {event_id} --owner"))
    );
}

/// 4.8: a spoke prints the block for a skip in its hub's log after every sync.
#[test]
fn a_spoke_reports_its_hubs_skip_every_time() {
    let hub = start_hub();
    let c = node("spokec");
    let (event_id, size) = hub_with_an_oversized_event(&hub, &c);
    let runs = [0, 1].map(|_| cli(&c, &["sync"]));
    hub.ct.cancel();
    let want = hub_block(&event_id, size, "node hub");
    for run in &runs {
        assert_eq!(run.status.code(), Some(0), "{}", err(run));
        let text = out(run);
        let (_, rest) = text.split_once('\n').unwrap();
        assert_eq!(rest, want);
    }
}

/// 4.8, through the CLI: a hub's marker for an event of another spoke sends the reader to
/// that spoke for the remedy and to the hub for the acknowledgement.
#[test]
fn a_hubs_marker_for_a_relayed_event_sends_the_reader_to_the_events_node() {
    let hub = start_hub();
    let s = node("spokes");
    let t = node("spoket");
    join(&hub, &s);
    join(&hub, &t);
    fed::sync(&t.store).unwrap();
    plant(
        &hub.node,
        "01PLANTEDRELAY",
        "sync.skipped",
        &marker_for("01RE1AYED00000000000000000", &id_of(&t), "pull"),
    );
    let on_s = cli(&s, &["sync"]);
    let on_t = cli(&t, &["sync"]);
    hub.ct.cancel();
    for (run, place) in [(&on_s, "node spoket"), (&on_t, "this node")] {
        assert_eq!(run.status.code(), Some(0), "{}", err(run));
        let text = out(run);
        let (_, rest) = text.split_once('\n').unwrap();
        assert_eq!(
            rest,
            hub_block("01RE1AYED00000000000000000", 9_437_500, place)
        );
    }
}

/// 4.8: at most five skips are printed in full, then a count.
#[test]
fn more_than_five_skips_print_five_and_a_count() {
    let hub = start_hub();
    let a = node("spokea");
    join(&hub, &a);
    let a_id = id_of(&a);
    for i in 0..7 {
        plant(
            &a,
            &format!("01PLANTED{i}"),
            "sync.skipped",
            &marker_for(&format!("01SK1PPED0000000000000000{i}"), &a_id, "push"),
        );
    }
    let run = cli(&a, &["sync"]);
    let as_json = cli(&a, &["--json", "sync"]);
    assert_eq!(run.status.code(), Some(0), "{}", err(&run));
    let text = out(&run);
    let blocks: Vec<&str> = text.lines().filter(|l| l.starts_with("skipped ")).collect();
    assert_eq!(blocks.len(), 5, "{text}");
    for (i, line) in blocks.iter().enumerate() {
        assert!(
            line.starts_with(&format!("skipped 01SK1PPED0000000000000000{i} (")),
            "{line}"
        );
    }
    assert!(
        text.ends_with("\nand 2 more skipped event(s): another needs\n"),
        "{text}"
    );
    // The JSON form is cut at the same five, with the count of all beside it.
    let v: Value = serde_json::from_str(&out(&as_json)).unwrap();
    let listed: Vec<&str> = v["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["event_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        listed,
        (0..5)
            .map(|i| format!("01SK1PPED0000000000000000{i}"))
            .collect::<Vec<_>>()
    );
    assert_eq!(v["skipped_total"], json!(7));

    // Many markers do not make the report large: with 300 more it is the same five
    // entries, in the text and in the JSON, and the JSON stays a few kilobytes.
    for i in 0..300 {
        plant(
            &a,
            &format!("01PLANTEDMORE{i:03}"),
            "sync.skipped",
            &marker_for(&format!("01M0RE00000000000000000{i:03}"), &a_id, "push"),
        );
    }
    let run = cli(&a, &["sync"]);
    let as_json = cli(&a, &["--json", "sync"]);
    let needs = cli(&a, &["--json", "needs", "--limit", "200"]);
    let feed = cli(&a, &["activity", "--kinds", "sync", "--limit", "500"]);
    hub.ct.cancel();
    assert!(
        out(&run).ends_with("\nand 302 more skipped event(s): another needs\n"),
        "{}",
        out(&run)
    );
    let v: Value = serde_json::from_str(&out(&as_json)).unwrap();
    assert_eq!(v["skipped"].as_array().unwrap().len(), 5);
    assert_eq!(v["skipped_total"], json!(307));
    assert!(out(&as_json).len() < 32 * 1024, "{}", out(&as_json).len());
    // `another needs --json` counts them all and lists up to its row limit, and
    // `another activity --kinds sync` lists every one.
    let v: Value = serde_json::from_str(&out(&needs)).unwrap();
    assert_eq!(v["counts"]["skipped_event"], json!(307));
    assert_eq!(
        v["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["kind"] == json!("skipped_event"))
            .count(),
        200
    );
    assert_eq!(out(&feed).lines().count(), 307, "{}", out(&feed));
}

/// A hub chooses every text in its markers, and it may mark an event as written by this
/// node, so the steps then read "on this node". A value from a marker is put into a
/// printed command only when it is a valid agent name or an id this software makes;
/// anything else leaves the placeholder and is said. Before this rule a hub's
/// `about: "hold ."` printed `another hold release --agent <agent> .`, which releases
/// every hold under the directory it is run in.
#[test]
fn a_hubs_marker_puts_no_text_of_its_own_into_a_printed_command() {
    let hub = start_hub();
    let c = node("spokec");
    join(&hub, &c);
    let c_id = id_of(&c);
    let mut hold = marker_for("01H01D00000000000000000000", &c_id, "pull");
    hold["event_kind"] = json!("hold.acquired");
    hold["about"] = json!("hold .");
    let mut agent = marker_for("--all", &c_id, "pull");
    agent["about"] = json!("agent x'; touch pwned; echo '");
    plant(&hub.node, "01PLANTEDHOLD", "sync.skipped", &hold);
    plant(&hub.node, "01PLANTEDNAME", "sync.skipped", &agent);
    let run = cli(&c, &["sync"]);
    let needs = cli(&c, &["needs"]);
    let as_json = cli(&c, &["--json", "sync"]);
    hub.ct.cancel();
    assert_eq!(run.status.code(), Some(0), "{}", err(&run));
    let text = out(&run);

    assert!(
        text.contains(
            "\n  about: hold .\n  note: the hold id in this report is not an id this software \
             makes, so <hold id> is left for you to fill\n"
        ),
        "{text}"
    );
    assert!(
        text.contains(
            "\n  to get it across, on this node:\n    another hold release --agent <agent> \
             <hold id>\n"
        ),
        "{text}"
    );
    assert!(
        text.contains(
            "\n  then, on node hub: another sync ack 01H01D00000000000000000000 --owner\n"
        ),
        "{text}"
    );
    assert!(
        text.contains(
            "\n  note: the agent's name in this report is not a valid agent name, so <name> \
             is left for you to fill\n  note: the event id in this report is not an id this \
             software makes, so <event-id> is left for you to fill\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("\n    another agent register <name> --program <program> "),
        "{text}"
    );
    assert!(
        text.contains("\n  then, on node hub: another sync ack <event-id> --owner\n"),
        "{text}"
    );
    // The hub's text is shown as what the report is about, and in no command.
    for (what, shown) in [("sync", text.as_str()), ("needs", &out(&needs))] {
        for line in shown.lines() {
            let commands = line.contains("another ") && !line.trim_start().starts_with("about:");
            assert!(
                !(commands && (line.contains("touch pwned") || line.contains("--all"))),
                "{what}: {line}"
            );
            assert!(!line.contains("<agent> ."), "{what}: {line}");
            assert!(
                !line.contains("release --agent <agent> `"),
                "{what}: {line}"
            );
        }
    }
    let needs = out(&needs);
    assert!(
        needs.contains(
            "    next: note: the hold id in this report is not an id this software makes, so \
             <hold id> is left for you to fill\n          release the hold: `another hold \
             release --agent <agent> <hold id>`\n"
        ),
        "{needs}"
    );
    assert!(
        needs.contains(
            "          on node hub: acknowledge it with `another sync ack <event-id> --owner`\n"
        ),
        "{needs}"
    );
    let v: Value = serde_json::from_str(&out(&as_json)).unwrap();
    let notes: Vec<usize> = v["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["notes"].as_array().map_or(0, Vec::len))
        .collect();
    assert_eq!(notes, [1, 2]);
}

/// R15, through the CLI: `--kinds sync` lists the skips and acknowledgements and nothing
/// else, however many syncs have run since; `--kinds federation` lists none of them.
#[test]
fn kinds_sync_lists_only_sync_events() {
    let hub = start_hub();
    let a = node("spokea");
    join(&hub, &a);
    register(&hub.node.store, "bob");
    register(&a.store, "alice");
    fed::sync(&a.store).unwrap();
    let a_id = id_of(&a);
    plant(
        &a,
        "01PLANTEDKIND",
        "sync.skipped",
        &marker_for("01K1ND00000000000000000000", &a_id, "push"),
    );
    a.store
        .acknowledge_skip("01K1ND00000000000000000000")
        .unwrap();
    for i in 0..60 {
        send(&a.store, "alice", "bob@@hub", &format!("m{i}"), "x".into());
        fed::sync(&a.store).unwrap();
    }
    let records = count(
        &a.db(),
        "SELECT count(*) FROM events WHERE kind LIKE 'peer.%'",
    );
    let sync = cli(&a, &["activity", "--kinds", "sync"]);
    let as_json = cli(&a, &["--json", "activity", "--kinds", "sync"]);
    let federation = cli(&a, &["activity", "--kinds", "federation", "--limit", "500"]);
    let unknown = cli(&a, &["activity", "--kinds", "nosuch"]);
    hub.ct.cancel();

    assert!(records > 50, "{records}");
    let lines: Vec<String> = out(&sync).lines().map(str::to_string).collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines[0].ends_with(
            "skipped event 01K1ND00000000000000000000 (agent.registered, 9437500 bytes, written by wordy): \
             too large to send; agent wordy"
        ),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].ends_with(
            "acknowledged the skip of event 01K1ND00000000000000000000; it stays in the log and is not sent"
        ),
        "{}",
        lines[1]
    );
    let v: Value = serde_json::from_str(&out(&as_json)).unwrap();
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert!(items.iter().all(|i| i["category"] == json!("sync")));
    let fed_lines = out(&federation);
    assert!(fed_lines.lines().count() > 50, "{fed_lines}");
    assert!(
        !fed_lines.contains("01K1ND00000000000000000000"),
        "{fed_lines}"
    );
    assert_eq!(unknown.status.code(), Some(1));
    assert_eq!(
        err(&unknown),
        "another: unknown activity kind \"nosuch\"; expected mail, request, hold, resource, \
         session, system, federation, queue or sync\n"
    );
}

/// Run the binary on a pseudo-terminal with `typed` as the keys pressed once the prompt is
/// on the screen. Returns what the terminal showed, with plain line ends, and the exit
/// status. `script` is the standard tool for this on macOS and Linux.
#[cfg(unix)]
fn in_terminal(n: &Node, args: &[&str], typed: &str) -> (String, Option<i32>) {
    use std::io::Write;
    let exe = env!("CARGO_BIN_EXE_another");
    let mut cmd = Command::new("script");
    if cfg!(target_os = "linux") {
        let quoted: Vec<String> = std::iter::once(exe)
            .chain(args.iter().copied())
            .map(|a| format!("'{}'", a.replace('\'', "'\\''")))
            .collect();
        cmd.args(["-qec", &quoted.join(" "), "/dev/null"]);
    } else {
        cmd.args(["-q", "/dev/null", exe]).args(args);
    }
    let mut child = cmd
        .env("ANOTHER_HOME", n.dir.path())
        .env_remove("ANOTHER_AGENT")
        .env_remove("ANOTHER_ADMIT_KEY")
        .env_remove("ANOTHER_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run script(1)");
    // Type only once the prompt is on the screen, as a person would, and keep the keyboard
    // open until the command has finished.
    let mut keyboard = child.stdin.take();
    let mut screen = child.stdout.take().unwrap();
    let mut shown = Vec::new();
    let mut byte = [0u8; 1];
    while let Ok(1) = screen.read(&mut byte) {
        shown.push(byte[0]);
        if shown.ends_with(b"Type yes to confirm: ") {
            if let Some(keys) = keyboard.as_mut() {
                keys.write_all(typed.as_bytes()).unwrap();
                keys.flush().unwrap();
            }
        }
    }
    drop(keyboard);
    let done = child.wait_with_output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&shown),
        String::from_utf8_lossy(&done.stderr)
    );
    (text.replace("\r\n", "\n"), done.status.code())
}

/// A node with one unacknowledged skip of its own: a marker in its log for `01SK1PPED00000000000000000`.
/// It has no hub: the acknowledgement needs none.
fn node_with_a_skip() -> Node {
    let n = node("solo");
    plant(
        &n,
        "01PLANTEDSOLO",
        "sync.skipped",
        &marker_for("01SK1PPED00000000000000000", &id_of(&n), "push"),
    );
    n
}

fn acknowledgements(n: &Node) -> (i64, i64) {
    (
        count(
            &n.db(),
            "SELECT count(*) FROM events WHERE kind = 'sync.skip_acked'",
        ),
        audit_rows(n, "sync ack"),
    )
}

/// R11: the acknowledgement is the owner's alone. It needs `--owner`, then a terminal on
/// standard input and standard error, then a typed `yes` or `y`. Every refusal writes
/// nothing and leaves the skip reported.
#[cfg(unix)]
#[test]
fn an_acknowledgement_needs_owner_a_terminal_and_a_typed_yes() {
    let n = node_with_a_skip();
    let untouched = |n: &Node, what: &str| {
        assert_eq!(acknowledgements(n), (0, 0), "{what}");
        assert_eq!(n.store.unacknowledged_skips().unwrap().len(), 1, "{what}");
    };

    let no_owner = cli(&n, &["sync", "ack", "01SK1PPED00000000000000000"]);
    assert_eq!(no_owner.status.code(), Some(1));
    assert_eq!(
        err(&no_owner),
        "another: another sync ack is the owner's alone: run it with --owner at a terminal\n"
    );
    assert!(out(&no_owner).is_empty());
    untouched(&n, "without --owner");
    // Even in a terminal: the flag is what is asked for.
    let (shown, code) = in_terminal(&n, &["sync", "ack", "01SK1PPED00000000000000000"], "yes\n");
    assert_eq!(code, Some(1), "{shown}");
    assert!(
        shown.contains("another sync ack is the owner's alone: run it with --owner at a terminal"),
        "{shown}"
    );
    assert!(!shown.contains("Type yes to confirm"), "{shown}");
    untouched(&n, "without --owner in a terminal");

    let no_terminal = cli(
        &n,
        &["sync", "ack", "01SK1PPED00000000000000000", "--owner"],
    );
    assert_eq!(no_terminal.status.code(), Some(1));
    assert_eq!(
        err(&no_terminal),
        "another: owner actions need an interactive terminal\n"
    );
    untouched(&n, "without a terminal");

    let prompt = "Acknowledge the skip of event 01SK1PPED00000000000000000 (agent.registered, 9437500 bytes, \
                  written by wordy) in this node's log.\n\
                  It was not sent because it is larger than one push. This does not send it \
                  and deletes nothing.\n\
                  another sync and another needs stop reporting it, here and on the nodes \
                  that received the report.\n\
                  Type yes to confirm: ";
    for typed in ["no\n", "\n", "yes please\n"] {
        let (shown, code) = in_terminal(
            &n,
            &["sync", "ack", "01SK1PPED00000000000000000", "--owner"],
            typed,
        );
        assert!(shown.starts_with(prompt), "{typed:?}: {shown}");
        assert!(
            shown.contains("another: not confirmed: nothing was changed"),
            "{typed:?}: {shown}"
        );
        assert_eq!(code, Some(1), "{typed:?}: {shown}");
        untouched(&n, typed);
    }

    let (shown, code) = in_terminal(
        &n,
        &["sync", "ack", "01SK1PPED00000000000000000", "--owner"],
        "yes\n",
    );
    assert!(shown.starts_with(prompt), "{shown}");
    assert!(
        shown.contains(
            "acknowledged: event 01SK1PPED00000000000000000 (agent.registered, 9437500 bytes) stays in this \
             node's log and is still not sent; another sync and another needs no longer \
             report it"
        ),
        "{shown}"
    );
    assert_eq!(code, Some(0), "{shown}");
    assert_eq!(acknowledgements(&n), (1, 1));
    assert!(n.store.unacknowledged_skips().unwrap().is_empty());

    // `y` is a yes too.
    let n = node_with_a_skip();
    let (shown, code) = in_terminal(
        &n,
        &["sync", "ack", "01SK1PPED00000000000000000", "--owner"],
        "y\n",
    );
    assert_eq!(code, Some(0), "{shown}");
    assert_eq!(acknowledgements(&n), (1, 1));
}

/// R11: exactly one id per call, and there is no `--all`.
#[test]
fn ack_takes_exactly_one_id_and_has_no_all() {
    let n = node_with_a_skip();
    for args in [
        vec![
            "sync",
            "ack",
            "01SK1PPED00000000000000000",
            "01OTHER",
            "--owner",
        ],
        vec!["sync", "ack", "--owner"],
        vec!["sync", "ack", "--all", "--owner"],
        vec![
            "sync",
            "ack",
            "01SK1PPED00000000000000000",
            "--all",
            "--owner",
        ],
    ] {
        let run = cli(&n, &args);
        assert_eq!(run.status.code(), Some(2), "{args:?}: {}", err(&run));
        assert!(out(&run).is_empty(), "{args:?}");
        assert_eq!(acknowledgements(&n), (0, 0), "{args:?}");
    }
    assert_eq!(n.store.unacknowledged_skips().unwrap().len(), 1);
}

/// R11 step 2: the id is looked up before anything is asked. An unknown id, and an event
/// in another node's log, are refused with no prompt, even with no terminal; an event
/// already acknowledged is reported and nothing is written.
#[test]
fn ack_refuses_before_asking_for_an_unknown_id_and_on_a_node_that_is_not_the_holder() {
    let hub = start_hub();
    let c = node("spokec");
    join(&hub, &c);
    let hub_id = id_of(&hub.node);
    plant(
        &hub.node,
        "01PLANTEDHUB",
        "sync.skipped",
        &marker_for("01HB5J0G000000000000000000", &hub_id, "pull"),
    );
    fed::sync(&c.store).unwrap();
    hub.ct.cancel();
    assert_eq!(c.store.unacknowledged_skips().unwrap().len(), 1);

    let unknown = cli(&c, &["sync", "ack", "01NOSUCH", "--owner"]);
    assert_eq!(unknown.status.code(), Some(1));
    assert_eq!(
        err(&unknown),
        "another: no skipped event 01NOSUCH; skipped events are listed by: another activity \
         --kinds sync\n"
    );
    let as_json = cli(&c, &["--json", "sync", "ack", "01NOSUCH", "--owner"]);
    assert_eq!(as_json.status.code(), Some(1));
    let v: Value = serde_json::from_str(out(&as_json).trim()).unwrap();
    assert_eq!(v["error"]["code"], json!("not_found"));

    let elsewhere = cli(
        &c,
        &["sync", "ack", "01HB5J0G000000000000000000", "--owner"],
    );
    assert_eq!(elsewhere.status.code(), Some(1));
    assert_eq!(
        err(&elsewhere),
        "another: event 01HB5J0G000000000000000000 is in node hub's log; acknowledge it there: another sync \
         ack 01HB5J0G000000000000000000 --owner\n"
    );
    let as_json = cli(
        &c,
        &[
            "--json",
            "sync",
            "ack",
            "01HB5J0G000000000000000000",
            "--owner",
        ],
    );
    assert_eq!(as_json.status.code(), Some(1));
    let v: Value = serde_json::from_str(out(&as_json).trim()).unwrap();
    assert_eq!(v["error"]["code"], json!("invalid"));
    assert_eq!(acknowledgements(&c), (0, 0));
    assert_eq!(c.store.unacknowledged_skips().unwrap().len(), 1);
    #[cfg(unix)]
    for id in ["01NOSUCH", "01HB5J0G000000000000000000"] {
        let (shown, code) = in_terminal(&c, &["sync", "ack", id, "--owner"], "yes\n");
        assert_eq!(code, Some(1), "{shown}");
        assert!(!shown.contains("Type yes to confirm"), "{shown}");
        assert_eq!(acknowledgements(&c), (0, 0));
    }

    // Already acknowledged, on the holder: said, exit 0, no prompt, nothing written.
    let n = node_with_a_skip();
    n.store
        .acknowledge_skip("01SK1PPED00000000000000000")
        .unwrap();
    let at: i64 = Connection::open(n.db())
        .unwrap()
        .query_row(
            "SELECT ts FROM events WHERE kind = 'sync.skip_acked'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let again = cli(
        &n,
        &["sync", "ack", "01SK1PPED00000000000000000", "--owner"],
    );
    assert_eq!(again.status.code(), Some(0), "{}", err(&again));
    assert_eq!(
        out(&again),
        format!(
            "event 01SK1PPED00000000000000000 was acknowledged at {}; nothing was changed\n",
            fmt_ts(at)
        )
    );
    assert!(err(&again).is_empty(), "{}", err(&again));
    assert_eq!(acknowledgements(&n), (1, 1));
}

#[cfg(unix)]
fn rows_of(n: &Node, sql: &str) -> Vec<String> {
    let c = Connection::open(n.db()).unwrap();
    let mut stmt = c.prepare(sql).unwrap();
    let columns = stmt.column_count();
    let rows = stmt
        .query_map([], |r| {
            let mut cells = Vec::new();
            for i in 0..columns {
                cells.push(format!("{:?}", r.get::<_, rusqlite::types::Value>(i)?));
            }
            Ok(cells.join("|"))
        })
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

/// R12, R13: the acknowledgement writes one event and one audit row and deletes nothing:
/// the skipped event, its marker and every earlier audit row are there, unchanged.
#[cfg(unix)]
#[test]
fn acknowledging_deletes_nothing_and_writes_one_event_and_one_audit_row() {
    let hub = start_hub();
    let a = node("spokea");
    let (event_id, size) = spoke_with_an_oversized_event(&hub, &a);
    fed::sync(&a.store).unwrap();
    hub.ct.cancel();
    // Every column of every row, the 9 MiB payload included, byte for byte.
    let events = "SELECT seq, event_id, node_id, ts, actor, kind, payload, prev_id, sig \
                  FROM events ORDER BY seq";
    let audit = "SELECT * FROM audit_log ORDER BY audit_id";
    let before = (rows_of(&a, events), rows_of(&a, audit));
    assert!(before.0.iter().any(|row| row.len() > 9 * MIB));
    assert_eq!(audit_rows(&a, "sync skip"), 1);

    let (shown, code) = in_terminal(&a, &["sync", "ack", &event_id, "--owner"], "yes\n");
    assert_eq!(code, Some(0), "{shown}");
    assert!(
        shown.contains(&format!(
            "acknowledged: event {event_id} (agent.registered, {size} bytes) stays in this \
             node's log and is still not sent; another sync and another needs no longer \
             report it"
        )),
        "{shown}"
    );

    let after = (rows_of(&a, events), rows_of(&a, audit));
    assert_eq!(after.0.len(), before.0.len() + 1);
    assert!(
        after.0[..before.0.len()] == before.0[..],
        "an event changed"
    );
    assert_eq!(after.1.len(), before.1.len() + 1);
    assert_eq!(after.1[..before.1.len()], before.1[..]);
    assert!(holds_event(&a, &event_id));

    let a_id = id_of(&a);
    let acked = payloads(&a, "sync.skip_acked", &a_id);
    assert_eq!(
        acked,
        [json!({ "event_id": event_id, "route": "directory", "route_nodes": [] })]
    );
    let (actor, operation, outcome, agent, detail): (String, String, String, String, String) =
        Connection::open(a.db())
            .unwrap()
            .query_row(
                "SELECT (SELECT actor FROM events WHERE kind = 'sync.skip_acked'),
                        operation, outcome, agent, detail
                 FROM audit_log ORDER BY audit_id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
    assert_eq!(actor, "human (another sync ack)");
    assert_eq!((operation.as_str(), outcome.as_str()), ("sync ack", "ok"));
    assert_eq!(agent, format!("node:{a_id}"));
    assert_eq!(
        detail,
        format!(
            "event {event_id} (agent.registered, {size} bytes) acknowledged with --owner at a \
             terminal; it stays in the log and is not sent"
        )
    );
}

/// R14: after the acknowledgement `another sync` and `another needs` are quiet, on the
/// holder at once and on a spoke after its next sync, and `another activity --kinds sync`
/// still shows the skip and the acknowledgement on both.
#[cfg(unix)]
#[test]
fn after_an_acknowledgement_sync_is_quiet_and_the_skip_stays_readable() {
    // A spoke that holds the event.
    let hub = start_hub();
    let a = node("spokea");
    let (event_id, size) = spoke_with_an_oversized_event(&hub, &a);
    let loud = cli(&a, &["sync"]);
    let (shown, code) = in_terminal(&a, &["sync", "ack", &event_id, "--owner"], "yes\n");
    let quiet = cli(&a, &["sync"]);
    let needs = cli(&a, &["needs", "--brief"]);
    let feed = cli(&a, &["activity", "--kinds", "sync"]);
    let hub_feed = cli(&hub.node, &["activity", "--kinds", "sync"]);
    hub.ct.cancel();
    assert!(out(&loud).contains("\nskipped "), "{}", out(&loud));
    assert_eq!(code, Some(0), "{shown}");
    assert_eq!(quiet.status.code(), Some(0));
    assert_eq!(out(&quiet).lines().count(), 1, "{}", out(&quiet));
    assert!(out(&needs).is_empty(), "{}", out(&needs));
    let skip_line = format!(
        "skipped event {event_id} (agent.registered, {size} bytes, written by wordy): too \
         large to send; agent wordy"
    );
    let ack_line =
        format!("acknowledged the skip of event {event_id}; it stays in the log and is not sent");
    let lines: Vec<String> = out(&feed).lines().map(str::to_string).collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].ends_with(&skip_line), "{}", lines[0]);
    assert!(lines[1].ends_with(&ack_line), "{}", lines[1]);
    // The hub received both in the sync after the acknowledgement.
    let lines: Vec<String> = out(&hub_feed).lines().map(str::to_string).collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines[0].ends_with(&format!("[spokea] {skip_line}")),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].ends_with(&format!("[spokea] {ack_line}")),
        "{}",
        lines[1]
    );

    // A hub that holds the event, and a spoke that reported the hub's skip.
    let hub = start_hub();
    let c = node("spokec");
    let (event_id, size) = hub_with_an_oversized_event(&hub, &c);
    let loud = cli(&c, &["sync"]);
    let hub_loud = cli(&hub.node, &["needs", "--brief"]);
    let (shown, code) = in_terminal(&hub.node, &["sync", "ack", &event_id, "--owner"], "yes\n");
    let hub_quiet = cli(&hub.node, &["needs", "--brief"]);
    let still = c.store.unacknowledged_skips().unwrap().len();
    let quiet = cli(&c, &["sync"]);
    let needs = cli(&c, &["needs", "--brief"]);
    let feed = cli(&c, &["activity", "--kinds", "sync"]);
    let hub_feed = cli(&hub.node, &["activity", "--kinds", "sync"]);
    hub.ct.cancel();
    assert!(out(&loud).contains("\nskipped "), "{}", out(&loud));
    assert_eq!(out(&hub_loud), "needs you: 1 (1 skipped event)\n");
    assert_eq!(code, Some(0), "{shown}");
    assert!(
        shown.contains("It was not sent because it is larger than one pull answer."),
        "{shown}"
    );
    assert!(out(&hub_quiet).is_empty(), "{}", out(&hub_quiet));
    assert_eq!(
        still, 1,
        "the spoke reports it until the acknowledgement arrives"
    );
    assert_eq!(out(&quiet).lines().count(), 1, "{}", out(&quiet));
    assert!(out(&needs).is_empty(), "{}", out(&needs));
    let skip_line = format!(
        "skipped event {event_id} (agent.registered, {size} bytes, written by wordy): too \
         large to send; agent wordy"
    );
    let ack_line =
        format!("acknowledged the skip of event {event_id}; it stays in the log and is not sent");
    let lines: Vec<String> = out(&hub_feed).lines().map(str::to_string).collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].ends_with(&skip_line), "{}", lines[0]);
    assert!(lines[1].ends_with(&ack_line), "{}", lines[1]);
    let lines: Vec<String> = out(&feed).lines().map(str::to_string).collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines[0].ends_with(&format!("[hub] {skip_line}")),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].ends_with(&format!("[hub] {ack_line}")),
        "{}",
        lines[1]
    );
}

/// 4.9: the audit rows of a skip and of an acknowledgement, as `another audit` prints them.
#[test]
fn the_audit_rows_have_the_stated_text() {
    let hub = start_hub();
    let a = node("spokea");
    let (pushed_id, pushed_size) = spoke_with_an_oversized_event(&hub, &a);
    register(&hub.node.store, "huge");
    let (pulled_id, pulled_size) = grow_last_registration(&hub.node, 33 * MIB);
    fed::sync(&a.store).unwrap();
    a.store.acknowledge_skip(&pushed_id).unwrap();
    let spoke_audit = out(&cli(&a, &["audit"]));
    let hub_audit = out(&cli(&hub.node, &["audit"]));
    let a_id = id_of(&a);
    let hub_id = id_of(&hub.node);
    hub.ct.cancel();

    let skip_row = format!(
        "  skipped node:{a_id} {:<32} event {pushed_id} (agent.registered, {pushed_size} bytes, \
         written by wordy) in this node's log was not sent: larger than one push (8388608 \
         bytes)",
        "sync skip"
    );
    let ack_row = format!(
        "  ok      node:{a_id} {:<32} event {pushed_id} (agent.registered, {pushed_size} bytes) \
         acknowledged with --owner at a terminal; it stays in the log and is not sent",
        "sync ack"
    );
    let hub_row = format!(
        "  skipped node:{hub_id} {:<32} event {pulled_id} (agent.registered, {pulled_size} \
         bytes, written by huge) in this node's log was not sent: larger than one pull answer \
         (33554432 bytes)",
        "sync skip"
    );
    for (audit, row) in [
        (&spoke_audit, &skip_row),
        (&spoke_audit, &ack_row),
        (&hub_audit, &hub_row),
    ] {
        assert_eq!(
            audit.lines().filter(|l| l.ends_with(row.as_str())).count(),
            1,
            "{row}\nin\n{audit}"
        );
    }
}

/// Section 7, with one side bounded as a build before D99 was. A spoke on this build skips
/// its 9 MiB event for an older hub too: the hub stores the marker and relays it, and mail
/// written after the event is delivered. A hub on this build skips its 33 MiB event for an
/// older spoke, whose sync then succeeds.
#[test]
fn a_skip_with_one_side_on_limits_before_d99() {
    let hub = start_hub_with(Some(Limits::BEFORE_D99));
    let a = node("spokea");
    let b = node("spokeb");
    let (event_id, _) = spoke_with_an_oversized_event(&hub, &a);
    join(&hub, &b);
    let pushed = fed::sync(&a.store).map(|r| r.push_rejected.len());
    let pulled = fed::sync(&b.store).map(|r| r.pull_rejected.len());
    let a_id = id_of(&a);
    let on_hub = markers(&hub.node, &a_id);
    let on_b = markers(&b, &a_id);
    let got = inbox_subjects(&hub.node.store, "bob");
    hub.ct.cancel();
    assert_eq!(pushed.map_err(|e| format!("{e:#}")), Ok(0));
    assert_eq!(pulled.map_err(|e| format!("{e:#}")), Ok(0));
    assert_eq!(on_hub.len(), 1);
    assert_eq!(on_hub[0]["event_id"], json!(event_id));
    assert_eq!(on_b, on_hub);
    assert_eq!(got, ["after", "before"]);

    let hub = start_hub();
    let c = node("spokec");
    let old = Limits::BEFORE_D99;
    join(&hub, &c);
    register(&hub.node.store, "bob");
    register(&c.store, "carol");
    fed::sync_with(&c.store, &old).unwrap();
    send(
        &hub.node.store,
        "bob",
        "carol@@spokec",
        "before",
        "small".into(),
    );
    register(&hub.node.store, "wordy");
    let (event_id, _) = grow_last_registration(&hub.node, 33 * MIB);
    send(
        &hub.node.store,
        "bob",
        "carol@@spokec",
        "after",
        "small".into(),
    );
    let pulled = fed::sync_with(&c.store, &old).map(|r| r.pull_rejected.len());
    let hub_id = id_of(&hub.node);
    let on_c = markers(&c, &hub_id);
    let got = inbox_subjects(&c.store, "carol");
    hub.ct.cancel();
    assert_eq!(pulled.map_err(|e| format!("{e:#}")), Ok(0));
    assert_eq!(on_c.len(), 1);
    assert_eq!(on_c[0]["event_id"], json!(event_id));
    assert_eq!(got, ["after", "before"]);
}

/// What one run of the relay scenario left behind.
struct Relayed {
    /// Spoke A's events as A, the hub and spoke C hold them, in each one's order.
    on_a: Vec<String>,
    on_hub: Vec<String>,
    on_c: Vec<String>,
    /// Requests the hub answered: pushes, pulls.
    trips: (i64, i64),
}

/// Spoke A writes thirty messages to an agent on spoke C, large and small in turn, and
/// they travel A to hub to C under `limits`.
fn relay_through_the_hub(limits: Limits) -> Relayed {
    let hub = start_hub_with(Some(limits));
    let a = node("spokea");
    let c = node("spokec");
    join(&hub, &a);
    join(&hub, &c);
    register(&c.store, "carol");
    register(&a.store, "alice");
    fed::sync_with(&c.store, &limits).unwrap();
    fed::sync_with(&a.store, &limits).unwrap();
    fed::sync_with(&c.store, &limits).unwrap();
    let before = round_trips(&hub);
    for i in 0..30 {
        let body = if i % 3 == 0 { 100 } else { BODY };
        send(
            &a.store,
            "alice",
            "carol@@spokec",
            &format!("m{i:02}"),
            "z".repeat(body),
        );
    }
    let pushed = fed::sync_with(&a.store, &limits).unwrap();
    assert_eq!(pushed.pushed, 30);
    assert!(pushed.push_rejected.is_empty());
    let pulled = fed::sync_with(&c.store, &limits).unwrap();
    assert_eq!(pulled.applied, 30);
    assert!(pulled.pull_rejected.is_empty());
    let after = round_trips(&hub);

    // Nothing is left to send and nothing arrives twice.
    let again = fed::sync_with(&a.store, &limits).unwrap();
    assert_eq!((again.pushed, again.push_duplicates), (0, 0));
    let again = fed::sync_with(&c.store, &limits).unwrap();
    assert_eq!(
        (again.pulled, again.applied, again.pull_duplicates),
        (0, 0, 0)
    );
    // The cursors are where one big batch would leave them: A's push cursor at its last
    // event that may leave the node, C's pull cursor at the hub's last event from others.
    let hub_id = hub.node.store.node_id().to_string();
    let a_id = a.store.node_id().to_string();
    let c_id = c.store.node_id().to_string();
    let last_shared = a
        .store
        .outbound_events_within(0, 10_000, usize::MAX)
        .unwrap()
        .0
        .last()
        .unwrap()
        .seq;
    assert!(a.store.push_cursor(&hub_id).unwrap().0 >= last_shared);
    assert_eq!(
        c.store.push_cursor(&hub_id).unwrap().1,
        count(
            &hub.node.db(),
            &format!("SELECT max(seq) FROM events WHERE node_id != '{c_id}'")
        )
    );
    assert_eq!(
        inbox_subjects(&c.store, "carol"),
        (0..30).map(|i| format!("m{i:02}")).collect::<Vec<_>>()
    );
    let out = Relayed {
        on_a: stored_from(&a, &a_id)
            .into_iter()
            .filter(|k| !k.starts_with("peer."))
            .collect(),
        on_hub: stored_from(&hub.node, &a_id),
        on_c: stored_from(&c, &a_id),
        trips: (after.0 - before.0, after.1 - before.1),
    };
    hub.ct.cancel();
    out
}

/// Ordering and cursors do not depend on how the events are cut into batches and pages:
/// through many small ones, and relayed by the hub to a second spoke, the same events
/// arrive in the same order as with one batch and one page, none skipped, none twice.
#[test]
fn many_batches_and_pages_keep_the_order_of_one_through_a_relay() {
    let wide = Limits {
        push_budget: usize::MAX,
        push_body: 64 * MIB,
        pull_budget: usize::MAX,
        pull_body: 64 * MIB,
        ..Limits::CURRENT
    };
    let narrow = Limits {
        push_budget: 600 * 1024,
        pull_budget: 600 * 1024,
        ..Limits::CURRENT
    };
    let one = relay_through_the_hub(wide);
    let many = relay_through_the_hub(narrow);
    // A's push of the thirty messages, and C's pull of them (one more pull each way is
    // the last, empty page or A's own pull).
    assert_eq!(one.trips.0, 1);
    assert!(many.trips.0 >= 10, "{:?}", many.trips);
    assert!(many.trips.1 >= one.trips.1 + 9, "{:?}", many.trips);
    for run in [&one, &many] {
        assert_eq!(run.on_hub, run.on_a);
        assert_eq!(run.on_c, run.on_a);
        assert_eq!(run.on_a.iter().filter(|k| k.starts_with('m')).count(), 30);
    }
    assert_eq!(many.on_c, one.on_c);
}

/// A body over a route's limit is refused with 413, signed or not, with or without a
/// declared length; a body under the limit goes on to the signature check. `/fed/pair`,
/// which nobody signs, keeps the 2 MiB it had. The pushes name a paired spoke, since a
/// push that names none is refused before its body is looked at (the next test).
#[test]
fn a_body_over_a_routes_limit_is_refused() {
    let hub = start_hub();
    let a = node("spokea");
    join(&hub, &a);
    let agent = http();
    // The status of the answer, or `None` when the hub closed the connection on the body
    // it would not read before the answer could be read (a refusal too; which I/O error
    // it shows as depends on the system).
    type Answer = Result<ureq::http::Response<ureq::Body>, ureq::Error>;
    let status = |answer: Answer| -> Option<u16> {
        match answer {
            Ok(resp) => Some(resp.status().as_u16()),
            Err(ureq::Error::Io(_)) => None,
            Err(e) => panic!("{e}"),
        }
    };
    let post = |path: &str, bytes: usize| -> Option<u16> {
        status(
            agent
                .post(&format!("{}{path}", hub.url))
                .header("content-type", "application/json")
                .header("x-another-node", a.store.node_id())
                .send(&vec![b' '; bytes][..]),
        )
    };
    // /fed/push: the limit is 8 MiB and is applied before the signature is checked.
    let over = post("/fed/push", PUSH_BODY_LIMIT_BYTES + 1);
    let at = post("/fed/push", PUSH_BODY_LIMIT_BYTES);
    // Without a declared length the hub stops reading at the limit.
    let streamed = status(
        agent
            .post(&format!("{}/fed/push", hub.url))
            .header("content-type", "application/json")
            .header("x-another-node", a.store.node_id())
            .send(ureq::SendBody::from_reader(
                &mut std::io::repeat(b' ').take(PUSH_BODY_LIMIT_BYTES as u64 + 1),
            )),
    );
    // Signed by a paired spoke: the same limit.
    let body = vec![b' '; PUSH_BODY_LIMIT_BYTES + 1];
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let signed = status(
        agent
            .post(&format!("{}/fed/push", hub.url))
            .header("x-another-node", a.store.node_id())
            .header("x-another-time", time.to_string())
            .header(
                "x-another-signature",
                a.store.sign_request("POST", "/fed/push", time, &body),
            )
            .send(&body[..]),
    );
    let pair_over = post("/fed/pair", fed::PAIR_BODY_LIMIT_BYTES + 1);
    let pair_at = post("/fed/pair", fed::PAIR_BODY_LIMIT_BYTES);
    hub.ct.cancel();
    for refused in [over, streamed, signed, pair_over] {
        assert!(matches!(refused, None | Some(413)), "{refused:?}");
    }
    assert_eq!(
        at,
        Some(401),
        "a body at the limit reaches the signature check"
    );
    let pair_at = pair_at.expect("a body at the limit is read");
    assert_ne!(pair_at, 413);
    assert!((400..500).contains(&pair_at), "{pair_at}");
}

/// Send the head of a push that declares a 4 MiB body, and none of the body. The status of
/// the answer, or `None` when the hub gives none within two seconds because it is waiting
/// for the body.
fn push_head_only(hub: &Hub, headers: &[(&str, String)]) -> Option<u16> {
    use std::io::Write;
    let addr = hub.url.strip_prefix("http://").unwrap();
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let mut head = format!(
        "POST /fed/push HTTP/1.1\r\nhost: {addr}\r\ncontent-type: application/json\r\n\
         content-length: {}\r\n",
        4 * MIB
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).unwrap();
    let mut answer = [0u8; 12];
    match stream.read_exact(&mut answer) {
        Ok(()) => std::str::from_utf8(&answer[9..12]).unwrap().parse().ok(),
        Err(_) => None,
    }
}

/// A push whose node header names no paired, unrevoked spoke is refused before the hub
/// reads any of its body, so a caller with no pairing makes the hub buffer nothing. A
/// paired spoke's push is read as before, whatever its time header says: that header is
/// checked with the signature, after the body.
#[test]
fn a_push_that_names_no_paired_spoke_is_refused_before_its_body_is_read() {
    let hub = start_hub();
    let a = node("spokea");
    let b = node("spokeb");
    join(&hub, &a);
    join(&hub, &b);
    hub.node.store.revoke_peer(b.store.node_id()).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let headers = |node: &str, time: i64| {
        vec![
            ("x-another-node", node.to_string()),
            ("x-another-time", time.to_string()),
            ("x-another-signature", "AAAA".to_string()),
        ]
    };
    let none = push_head_only(&hub, &[]);
    let unknown = push_head_only(&hub, &headers("01UNKNOWNNODE0000000000000", now));
    let revoked = push_head_only(&hub, &headers(b.store.node_id(), now));
    let paired = push_head_only(&hub, &headers(a.store.node_id(), now));
    let paired_stale = push_head_only(&hub, &headers(a.store.node_id(), now - 3_600_000));
    let denied = count(
        &hub.node.db(),
        "SELECT count(*) FROM audit_log WHERE operation = 'POST /fed/push' AND outcome = 'denied'",
    );
    hub.ct.cancel();
    assert_eq!(none, Some(401));
    assert_eq!(unknown, Some(401));
    assert_eq!(revoked, Some(401));
    assert_eq!(paired, None, "the hub waits for a paired spoke's body");
    assert_eq!(paired_stale, None);
    assert_eq!(denied, 2);
}

/// Halving goes all the way down to one event: two events of 200 KiB to a hub that reads
/// 300 KiB are refused together and delivered one at a time.
#[test]
fn a_refused_batch_of_two_is_sent_one_event_at_a_time() {
    let hub = start_hub_with(Some(Limits {
        push_body: 300 * 1024,
        ..Limits::BEFORE_D99
    }));
    let a = node("spokea");
    join(&hub, &a);
    register(&hub.node.store, "bob");
    register(&a.store, "alice");
    fed::sync(&a.store).unwrap();
    for subject in ["one", "two"] {
        send(
            &a.store,
            "alice",
            "bob@@hub",
            subject,
            "x".repeat(200 * 1024),
        );
    }
    let before = round_trips(&hub).0;
    let pushed = fed::sync(&a.store).map(|r| r.pushed);
    let trips = round_trips(&hub).0 - before;
    let bob = inbox_subjects(&hub.node.store, "bob");
    hub.ct.cancel();
    assert_eq!(pushed.map_err(|e| format!("{e:#}")), Ok(2));
    assert_eq!(bob, ["one", "two"]);
    assert_eq!(trips, 2);
}

/// A spoke on this build with a hub on a build before D99 (2 MiB request limit, pages
/// bounded by count only). A push the hub answers with 413 is cut in half and sent again,
/// so the nine messages that stalled arrive; a page is asked for by count, so a spoke that
/// cannot read one asks for half as many events.
#[test]
fn a_new_spoke_syncs_with_a_hub_on_a_build_before_d99() {
    let hub = start_hub_with(Some(Limits::BEFORE_D99));
    let a = node("spokea");
    join(&hub, &a);
    register(&hub.node.store, "bob");
    register(&a.store, "alice");
    fed::sync(&a.store).unwrap();
    for i in 0..9 {
        send(
            &a.store,
            "alice",
            "bob@@hub",
            &format!("big {i}"),
            "x".repeat(BODY),
        );
    }
    let before = round_trips(&hub);
    let pushed = fed::sync(&a.store).map(|r| r.pushed);
    let push_trips = round_trips(&hub).0 - before.0;
    let got = inbox_subjects(&hub.node.store, "bob");

    // Pull: 45 of the largest messages are 11.8 MiB in one page from this hub. This build
    // reads up to 32 MiB, so the page that stalled a build before D99 is read whole.
    for i in 0..45 {
        send(
            &hub.node.store,
            "bob",
            "alice@@spokea",
            &format!("pull {i:02}"),
            "y".repeat(BODY),
        );
    }
    let before = round_trips(&hub);
    let pulled = fed::sync(&a.store).map(|r| r.applied);
    let whole_page_trips = round_trips(&hub).1 - before.1;

    // A second spoke that reads at most 2 MiB stands for a page over the 32 MiB limit: it
    // halves the count it asks for until a page fits, and receives everything in order.
    let c = node("spokec");
    join(&hub, &c);
    register(&c.store, "carol");
    let small = Limits {
        pull_body: 2 * MIB,
        ..Limits::CURRENT
    };
    fed::sync_with(&c.store, &small).unwrap();
    for i in 0..45 {
        send(
            &hub.node.store,
            "bob",
            "carol@@spokec",
            &format!("page {i:02}"),
            "y".repeat(BODY),
        );
    }
    let halved = fed::sync_with(&c.store, &small).map(|r| r.applied);
    let carol: Vec<String> = stored_from(&c, hub.node.store.node_id())
        .into_iter()
        .filter(|k| k.starts_with("page "))
        .collect();
    hub.ct.cancel();

    assert_eq!(pushed.map_err(|e| format!("{e:#}")), Ok(9));
    assert_eq!(got.len(), 9);
    assert!(push_trips >= 2, "{push_trips}");
    assert_eq!(pulled.map_err(|e| format!("{e:#}")), Ok(45));
    assert_eq!(whole_page_trips, 1);
    assert_eq!(halved.map_err(|e| format!("{e:#}")), Ok(45));
    assert_eq!(
        carol,
        (0..45).map(|i| format!("page {i:02}")).collect::<Vec<_>>()
    );
}

/// Where halving ends: one event that the other side's limit cannot take. The sync fails
/// with a message that names the cause and says that retrying will not help. No build
/// writes an event over 2 MiB any more, so a hub and a spoke with far smaller limits stand
/// in for the case.
#[test]
fn one_event_over_the_other_sides_limit_is_reported_not_retried_forever() {
    let tiny = Limits {
        push_body: 100 * 1024,
        ..Limits::BEFORE_D99
    };
    let hub = start_hub_with(Some(tiny));
    let a = node("spokea");
    join(&hub, &a);
    register(&hub.node.store, "bob");
    register(&a.store, "alice");
    fed::sync(&a.store).unwrap();
    send(&a.store, "alice", "bob@@hub", "one", "x".repeat(BODY));
    let event_id: String = Connection::open(a.db())
        .unwrap()
        .query_row(
            "SELECT event_id FROM events WHERE kind = 'message.sent'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let push = format!("{:#}", fed::sync(&a.store).unwrap_err());

    let c = node("spokec");
    join(&hub, &c);
    register(&c.store, "carol");
    let deaf = Limits {
        pull_body: 100 * 1024,
        ..Limits::CURRENT
    };
    fed::sync_with(&c.store, &deaf).unwrap();
    send(
        &hub.node.store,
        "bob",
        "carol@@spokec",
        "two",
        "y".repeat(BODY),
    );
    let cursor = c.store.push_cursor(hub.node.store.node_id()).unwrap().1;
    let pull = format!("{:#}", fed::sync_with(&c.store, &deaf).unwrap_err());
    hub.ct.cancel();

    // The hub answers 413 without reading the body and closes. The spoke sees the answer,
    // or (on Linux, usually) the closed connection; each has its message.
    let refused = format!(
        "the hub at {} refused event {event_id} (message.sent) as too large: it alone makes \
         a push of ",
        hub.url
    );
    let dropped = format!(
        "the connection to the hub at {} failed while event {event_id} (message.sent) was \
         sent alone in a push of ",
        hub.url
    );
    if push.starts_with(&refused) {
        assert!(
            push.contains(
                " bytes, over what the hub reads in one request; retrying will not help; the \
                 hub runs a build with a smaller limit than this node's, so update the hub \
                 (push failed (413 Payload Too Large): "
            ),
            "{push}"
        );
    } else {
        assert!(push.starts_with(&dropped), "{push}");
        assert!(
            push.contains(
                " bytes; if this happens every time, the hub is refusing it as too large and \
                 retrying will not help: the hub runs a build with a smaller limit than this \
                 node's, so update the hub (cannot reach the hub at "
            ),
            "{push}"
        );
    }
    assert_eq!(
        pull,
        format!(
            "the next event the hub at {} holds for this node, after its sequence number \
             {cursor}, is by itself larger than this node reads in one answer; retrying will \
             not help; the hub runs an older build or holds an event over the size limit; a \
             hub on this build or later steps past such an event and reports it, so update \
             the hub (the pull response is larger than the 102400 bytes this node reads)",
            hub.url
        )
    );
}

/// A spoke on a build before D99 (batches by count only, answers read up to 10 MiB) with a
/// hub on this build: the two cases that stalled now sync, because the hub reads an 8 MiB
/// push and closes a page at 8 MiB.
#[test]
fn an_older_spoke_syncs_with_a_hub_on_this_build() {
    let old = Limits::BEFORE_D99;
    let hub = start_hub();
    let a = node("spokea");
    join(&hub, &a);
    register(&hub.node.store, "bob");
    register(&a.store, "alice");
    fed::sync_with(&a.store, &old).unwrap();
    for i in 0..9 {
        send(
            &a.store,
            "alice",
            "bob@@hub",
            &format!("big {i}"),
            "x".repeat(BODY),
        );
    }
    for i in 0..45 {
        send(
            &hub.node.store,
            "bob",
            "alice@@spokea",
            &format!("pull {i}"),
            "y".repeat(BODY),
        );
    }
    let before = round_trips(&hub);
    let report = fed::sync_with(&a.store, &old).map(|r| (r.pushed, r.applied));
    let trips = round_trips(&hub);
    let bob = inbox_subjects(&hub.node.store, "bob").len();
    let alice = inbox_subjects(&a.store, "alice").len();
    hub.ct.cancel();
    assert_eq!(report.map_err(|e| format!("{e:#}")), Ok((9, 45)));
    assert_eq!((bob, alice), (9, 45));
    // The nine went in one request, as the older spoke sends them.
    assert_eq!(trips.0 - before.0, 1);
}

/// Requests the hub answers while 2,000 small messages travel from spoke A through the hub
/// to spoke C, under `limits` on all three: (pushes by A, pulls by C).
fn trips_for_two_thousand(limits: Limits) -> (i64, i64) {
    let hub = start_hub_with(Some(limits));
    let a = node("spokea");
    let c = node("spokec");
    join(&hub, &a);
    join(&hub, &c);
    register(&c.store, "carol");
    register(&a.store, "alice");
    fed::sync_with(&c.store, &limits).unwrap();
    fed::sync_with(&a.store, &limits).unwrap();
    fed::sync_with(&c.store, &limits).unwrap();
    for i in 0..2000 {
        send(
            &a.store,
            "alice",
            "carol@@spokec",
            &format!("s{i}"),
            "small".into(),
        );
    }
    let start = round_trips(&hub);
    let pushed = fed::sync_with(&a.store, &limits).unwrap();
    let mid = round_trips(&hub);
    let pulled = fed::sync_with(&c.store, &limits).unwrap();
    let end = round_trips(&hub);
    hub.ct.cancel();
    assert_eq!((pushed.pushed, pulled.applied), (2000, 2000));
    (mid.0 - start.0, end.1 - mid.1)
}

/// The byte bound costs ordinary traffic nothing: 2,000 small events take the same number
/// of requests as on a build before D99.
#[test]
fn two_thousand_small_events_take_no_more_round_trips_than_before() {
    let before = trips_for_two_thousand(Limits::BEFORE_D99);
    let after = trips_for_two_thousand(Limits::CURRENT);
    eprintln!(
        "round trips for 2,000 small events (pushes, pulls): before {before:?}, after {after:?}"
    );
    assert_eq!(after, before);
    // Five each way: a batch scans 500 of the node's own rows, the local records of each
    // sync included, and the last full page is followed by an empty one.
    assert_eq!(after, (5, 5));
}

/// A shared port resource `hubsvc` of agent `owner` on the hub, with something listening
/// on it so a spoke can reach it. Returns the resource id and the listener.
fn hub_resource(hub: &Hub) -> (String, std::net::TcpListener) {
    register(&hub.node.store, "owner");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let id = hub
        .node
        .store
        .register_resource(RegisterResource {
            agent: "owner".into(),
            kind: ResourceKind::Port,
            name: "hubsvc".into(),
            endpoint: Some(format!("http://127.0.0.1:{port}")),
            port: Some(port),
            pid: None,
            project: None,
            purpose: None,
            mode: LeaseMode::Shared,
            ttl_seconds: None,
            metadata: None,
        })
        .unwrap()
        .resource
        .resource_id;
    (id, listener)
}

/// Run the binary with `--json` in `cwd` on `n`'s home; the command must succeed.
fn run_json(n: &Node, cwd: &std::path::Path, args: &[&str]) -> Value {
    let o = Command::new(env!("CARGO_BIN_EXE_another"))
        .arg("--json")
        .args(args)
        .current_dir(cwd)
        .env("ANOTHER_HOME", n.dir.path())
        .env_remove("ANOTHER_AGENT")
        .env_remove("ANOTHER_LOG")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(o.status.success(), "{args:?}: {}{}", out(&o), err(&o));
    serde_json::from_str(&out(&o)).unwrap()
}

/// 4.10, how a resource is named in the remedy steps: `kind:name` names only a resource of
/// the node the command runs on, so for another node's resource it is refused, by borrow
/// and by release, and the id `another resources` prints in brackets is what works.
#[test]
fn kind_and_name_is_refused_for_another_nodes_resource_and_its_id_is_taken() {
    let hub = start_hub();
    let s = node("spokes");
    join(&hub, &s);
    let (resource_id, _listener) = hub_resource(&hub);
    register(&s.store, "wordy");
    fed::sync(&s.store).unwrap();
    let state = |n: &Node| {
        (
            count(&n.db(), "SELECT count(*) FROM events"),
            count(&n.db(), "SELECT count(*) FROM resource_leases"),
        )
    };
    let before = state(&s);
    for verb in ["borrow", "release"] {
        let run = cli(&s, &[verb, "port:hubsvc", "--agent", "wordy"]);
        assert_eq!(run.status.code(), Some(1), "{verb}");
        assert_eq!(
            err(&run).lines().next(),
            Some("another: no registered resource \"port:hubsvc\""),
            "{verb}"
        );
        assert!(out(&run).is_empty(), "{verb}");
        assert_eq!(state(&s), before, "{verb}");
    }
    let listed = out(&cli(&s, &["resources"]));
    let line = listed.lines().find(|l| l.contains("hubsvc")).unwrap();
    assert!(
        line.trim_end().ends_with(&format!("[{resource_id}]")),
        "{line}"
    );
    let borrowed = run_json(
        &s,
        s.dir.path(),
        &["borrow", &resource_id, "--agent", "wordy"],
    );
    hub.ct.cancel();
    assert_eq!(borrowed["outcome"], json!("borrowed"));
    assert_eq!(borrowed["lease"]["role"], json!("borrower"));
}

/// 4.10, the six steps for a skipped first registration, run as written through the CLI
/// with real syncs. An agent with a model, a shared hold with a reason, a resource of its
/// own and a borrow on the hub's resource; its first registration is too large to send.
/// Registering again brings the agent, acquiring again the hold, releasing and borrowing
/// again the leases. A borrow without the release brings nothing. Mail it sent other nodes
/// before it registered again is not delivered (asserted, so the loss stays documented).
#[test]
fn the_steps_for_a_skipped_first_registration_restore_all_but_the_mail() {
    let hub = start_hub();
    let s = node("spokes");
    join(&hub, &s);
    register(&hub.node.store, "bob");
    let (hub_resource_id, _listener) = hub_resource(&hub);
    fed::sync(&s.store).unwrap();
    let project = s.dir.path().join("proj");
    let elsewhere = s.dir.path().join("elsewhere");
    for dir in [&project, &elsewhere] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let project_path = project.to_string_lossy().into_owned();
    let hold = [
        "hold",
        "acquire",
        "--agent",
        "wordy",
        "-p",
        &project_path,
        "--shared",
        "--reason",
        "why",
        "src/a.rs",
    ];

    // The agent's first registration, grown as an earlier build could have left it, and
    // everything the agent then did.
    run_json(
        &s,
        &elsewhere,
        &[
            "agent",
            "register",
            "wordy",
            "--program",
            "claude",
            "--model",
            "m1",
            "--description",
            "first",
        ],
    );
    let (event_id, _) = grow_last_registration(&s, 9 * MIB);
    run_json(&s, &project, &hold);
    run_json(
        &s,
        &elsewhere,
        &[
            "resource", "register", "port", "ownsvc", "--agent", "wordy", "--port", "6301",
        ],
    );
    let first_borrow = run_json(
        &s,
        &elsewhere,
        &["borrow", &hub_resource_id, "--agent", "wordy"],
    );
    assert_eq!(first_borrow["outcome"], json!("borrowed"));
    run_json(
        &s,
        &elsewhere,
        &[
            "send",
            "--from",
            "wordy",
            "--to",
            "bob@@hub",
            "--subject",
            "early",
            "--body",
            "x",
        ],
    );
    let synced = cli(&s, &["sync"]);
    assert!(
        out(&synced).contains(&format!("\nskipped {event_id} (agent.registered, ")),
        "{}{}",
        out(&synced),
        err(&synced)
    );

    let wordy = s.store.agent("wordy").unwrap().agent_id;
    let own_resource_id: String = Connection::open(s.db())
        .unwrap()
        .query_row(
            "SELECT resource_id FROM resources WHERE name = 'ownsvc'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let hold_id = |n: &Node| -> Vec<(String, i64, Option<String>)> {
        let c = Connection::open(n.db()).unwrap();
        let mut stmt = c
            .prepare(
                "SELECT hold_id, exclusive, reason FROM holds
                 WHERE agent_id = ?1 AND released_at IS NULL",
            )
            .unwrap();
        let rows = stmt
            .query_map([&wordy], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    };
    let leases = |n: &Node| -> Vec<(String, String)> {
        let c = Connection::open(n.db()).unwrap();
        let mut stmt = c
            .prepare(
                "SELECT resource_id, role FROM resource_leases
                 WHERE agent_id = ?1 AND released_at IS NULL ORDER BY role",
            )
            .unwrap();
        let rows = stmt
            .query_map([&wordy], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    };
    let on_hub = |column: &str| -> Option<String> {
        Connection::open(hub.node.db())
            .unwrap()
            .query_row(
                &format!("SELECT {column} FROM agents WHERE agent_id = ?1"),
                [&wordy],
                |r| r.get(0),
            )
            .ok()
    };
    let the_hold = hold_id(&s);
    assert_eq!(the_hold.len(), 1);
    assert_eq!((the_hold[0].1, the_hold[0].2.as_deref()), (0, Some("why")));
    assert_eq!(leases(&s).len(), 2);
    // Other nodes do not know the agent at all.
    assert_eq!(on_hub("name"), None);
    assert!(hold_id(&hub.node).is_empty());
    assert!(leases(&hub.node).is_empty());
    assert!(inbox_subjects(&hub.node.store, "bob").is_empty());

    // Step 1: register again, with the model. After it and a heartbeat the hub knows the
    // agent and its model, and still none of its hold or leases.
    run_json(
        &s,
        &elsewhere,
        &[
            "agent",
            "register",
            "wordy",
            "--program",
            "claude",
            "--model",
            "m1",
            "--description",
            "short",
        ],
    );
    run_json(&s, &elsewhere, &["heartbeat", "--agent", "wordy"]);
    run_json(&s, &elsewhere, &["sync"]);
    assert_eq!(on_hub("name").as_deref(), Some("wordy"));
    assert_eq!(on_hub("model").as_deref(), Some("m1"));
    assert!(hold_id(&hub.node).is_empty());
    assert!(leases(&hub.node).is_empty());

    // Step 2, as written, from another directory: the same hold, shared, with its reason.
    run_json(&s, &elsewhere, &hold);
    run_json(&s, &elsewhere, &["sync"]);
    assert_eq!(hold_id(&s), the_hold);
    assert_eq!(hold_id(&hub.node), the_hold);

    // A borrow without the release only renews: still no lease on the hub.
    for target in ["port:ownsvc", hub_resource_id.as_str()] {
        let renewed = run_json(&s, &elsewhere, &["borrow", target, "--agent", "wordy"]);
        assert_eq!(renewed["outcome"], json!("renewed"), "{target}");
    }
    run_json(&s, &elsewhere, &["sync"]);
    assert!(leases(&hub.node).is_empty());

    // Steps 3 and 4: release, then borrow. The own resource comes back with its owner
    // lease, the hub's resource with the borrow.
    for (target, outcome) in [
        ("port:ownsvc", "adopted"),
        (hub_resource_id.as_str(), "borrowed"),
    ] {
        let released = run_json(&s, &elsewhere, &["release", target, "--agent", "wordy"]);
        assert_eq!(released["outcome"], json!("released"), "{target}");
        let again = run_json(&s, &elsewhere, &["borrow", target, "--agent", "wordy"]);
        assert_eq!(again["outcome"], json!(outcome), "{target}");
    }
    run_json(&s, &elsewhere, &["sync"]);
    assert_eq!(
        leases(&hub.node),
        [
            (hub_resource_id.clone(), "borrower".to_string()),
            (own_resource_id, "owner".to_string()),
        ]
    );

    // Step 5: mail sent before step 1 is not delivered there; mail sent after is.
    assert!(inbox_subjects(&hub.node.store, "bob").is_empty());
    run_json(
        &s,
        &elsewhere,
        &[
            "send",
            "--from",
            "wordy",
            "--to",
            "bob@@hub",
            "--subject",
            "late",
            "--body",
            "x",
        ],
    );
    run_json(&s, &elsewhere, &["sync"]);
    let got = inbox_subjects(&hub.node.store, "bob");
    hub.ct.cancel();
    assert_eq!(got, ["late"]);
}
