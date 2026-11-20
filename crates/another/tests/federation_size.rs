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
    ct: CancellationToken,
    _rt: tokio::runtime::Runtime,
}

/// A hub on this build, served as `another serve --federation` serves it.
fn start_hub() -> Hub {
    start_hub_with(None)
}

/// With limits given, a hub that serves only the federation routes under them:
/// `Limits::BEFORE_D99` is a hub on a build before D99.
fn start_hub_with(limits: Option<Limits>) -> Hub {
    let node = node("hub");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let ct = CancellationToken::new();
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
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
        ct,
        _rt: rt,
    }
}

fn join(hub: &Hub, spoke: &Node) {
    let code = hub
        .node
        .store
        .create_invite(&hub.url, &Perm::ALL, 3600)
        .unwrap();
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
    hub.ct.cancel();
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

/// What still blocks: an event in a spoke's log that does not fit a push even alone (here
/// 9 MiB, over the 8 MiB a hub reads). A build before D99 blocked on it too, at 2 MiB.
/// What happens to it is the owner's decision and is not made here. The sync sends what
/// came before it, then fails with a message that names the event, says that retrying will
/// not help and that the owner must decide, every time, and `another sync` prints it and
/// exits non-zero. The event is neither skipped nor removed.
#[test]
fn an_event_over_the_push_limit_in_a_spokes_log_blocks_with_a_message_that_names_it() {
    let hub = start_hub();
    let a = node("spokea");
    join(&hub, &a);
    register(&hub.node.store, "bob");
    register(&a.store, "alice");
    fed::sync(&a.store).unwrap();
    send(&a.store, "alice", "bob@@hub", "before", "small".into());
    register(&a.store, "wordy");
    let (event_id, size) = grow_last_registration(&a, 9 * MIB);
    send(&a.store, "alice", "bob@@hub", "after", "small".into());

    let first = format!("{:#}", fed::sync(&a.store).unwrap_err());
    let second = format!("{:#}", fed::sync(&a.store).unwrap_err());
    let cli = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["sync"])
        .env("ANOTHER_HOME", a.dir.path())
        .env_remove("ANOTHER_AGENT")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let got = inbox_subjects(&hub.node.store, "bob");
    let kept = count(
        &a.db(),
        &format!("SELECT count(*) FROM events WHERE event_id = '{event_id}'"),
    );
    // The way out the docs give the owner today: remove the event from this node's log by
    // hand. The next sync then sends everything after it; peers never see that event.
    Connection::open(a.db())
        .unwrap()
        .execute("DELETE FROM events WHERE event_id = ?1", [&event_id])
        .unwrap();
    let after_removal = fed::sync(&a.store).map(|r| r.push_rejected.len());
    let delivered = inbox_subjects(&hub.node.store, "bob");
    let wordy_on_hub = hub.node.store.agent("wordy@@spokea").is_ok();
    hub.ct.cancel();

    assert_eq!(kept, 1);
    assert_eq!(after_removal.map_err(|e| format!("{e:#}")), Ok(0));
    assert_eq!(delivered, ["after", "before"]);
    assert!(!wordy_on_hub);
    let want = format!(
        "event {event_id} (agent.registered, {size} bytes on the wire) in this node's log is \
         larger than a hub reads in one push (8388608 bytes), so it cannot be sent and the \
         events after it wait behind it; retrying will not help; the owner must decide \
         whether it is skipped or keeps blocking (docs/FEDERATION.md, \"If a sync reports an \
         event that cannot be sent\")"
    );
    assert!(size > 9 * MIB);
    assert_eq!(first, want);
    assert_eq!(second, want);
    assert!(!cli.status.success());
    assert_eq!(
        String::from_utf8_lossy(&cli.stderr),
        format!("another: {want}\n")
    );
    assert!(cli.stdout.is_empty());
    assert_eq!(got, ["before"]);
    assert_eq!(
        count(
            &hub.node.db(),
            &format!("SELECT count(*) FROM events WHERE event_id = '{event_id}'")
        ),
        0
    );
}

/// The same on the hub's side: an event in the hub's own log that does not fit a pull
/// answer even alone (here 33 MiB, over the 32 MiB a spoke reads; a build before D99
/// blocked at 10 MiB) is not sent. A spoke receives what came before it, then its sync
/// fails with the hub's message.
#[test]
fn an_event_over_the_pull_limit_in_the_hubs_log_blocks_with_a_message_that_names_it() {
    let hub = start_hub();
    let c = node("spokec");
    join(&hub, &c);
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
    let (event_id, size) = grow_last_registration(&hub.node, 33 * MIB);
    send(
        &hub.node.store,
        "bob",
        "carol@@spokec",
        "after",
        "small".into(),
    );

    let first = format!("{:#}", fed::sync(&c.store).unwrap_err());
    let second = format!("{:#}", fed::sync(&c.store).unwrap_err());
    let got = inbox_subjects(&c.store, "carol");
    hub.ct.cancel();
    let want = format!(
        "pull failed (409 Conflict): event {event_id} (agent.registered, {size} bytes on the \
         wire) in the hub's log is larger than a spoke reads in one pull answer (33554432 \
         bytes), so it cannot be sent and the events after it wait behind it; retrying will \
         not help; the owner must decide whether it is skipped or keeps blocking \
         (docs/FEDERATION.md, \"If a sync reports an event that cannot be sent\")"
    );
    assert_eq!(first, want);
    assert_eq!(second, want);
    assert_eq!(got, ["before"]);
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
             not help; the hub runs an older build or holds an event over the size limit, \
             and its owner must decide what happens to that event (the pull response is \
             larger than the 102400 bytes this node reads)",
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
