//! An event too large to send is skipped with a marker (Q26b, D-next): where a marker and
//! its acknowledgement travel, who may write one, what a receiver refuses, what a node
//! reports, and what the remedy steps do to leases. Nodes are stores in temp directories
//! with manual clocks, paired and synced without HTTP, by the calls `another sync` makes.

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

use another_core::*;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

const T0: i64 = 1_800_000_000_000;
const MIB: usize = 1024 * 1024;

static FORGED: AtomicU64 = AtomicU64::new(0);

struct Node {
    dir: tempfile::TempDir,
    store: Store,
    clock: Arc<AtomicI64>,
}

fn node(name: &str) -> Node {
    let dir = tempfile::tempdir().unwrap();
    let (clock, handle) = Clock::manual(T0);
    let store = Store::open_with_clock(dir.path(), clock).unwrap();
    store.set_node_name(name).unwrap();
    Node {
        dir,
        store,
        clock: handle,
    }
}

impl Node {
    fn db(&self) -> PathBuf {
        self.dir.path().join(DB_FILE)
    }

    fn sql(&self) -> Connection {
        Connection::open(self.db()).unwrap()
    }

    fn id(&self) -> String {
        self.store.node_id().to_string()
    }

    fn count(&self, sql: &str) -> i64 {
        self.sql().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn project(&self, name: &str) -> String {
        let dir = self.dir.path().join("projects").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.to_string_lossy().into_owned();
        self.store.ensure_project("system", &path).unwrap();
        path
    }

    fn register(&self, name: &str) -> Agent {
        self.store
            .register_agent(RegisterAgent {
                name: name.into(),
                program: Program::Claude,
                project: None,
                model: None,
                description: None,
            })
            .unwrap()
    }

    fn key(&self) -> String {
        self.sql()
            .query_row("SELECT secret_key FROM nodes WHERE is_local = 1", [], |r| {
                r.get(0)
            })
            .unwrap()
    }

    /// A correctly signed event of this node, as it would travel. It is not in any log.
    fn signed(&self, kind: &str, payload: &Value) -> WireEvent {
        let n = FORGED.fetch_add(1, Ordering::SeqCst);
        let key = crypto::decode_secret(&self.key()).unwrap();
        let payload = payload.to_string();
        let node_id = self.id();
        let event_id = format!("forged-{n}");
        let ts = T0 + n as i64;
        let sig = crypto::sign(
            &key,
            &crypto::event_message(&event_id, &node_id, ts, "system", kind, &payload),
        );
        WireEvent {
            seq: 1_000_000 + n as i64,
            event_id,
            node_id,
            ts,
            actor: "system".into(),
            kind: kind.into(),
            payload,
            prev_id: None,
            sig,
        }
    }

    /// Give the newest local event of `kind` a field of `bytes` bytes, signed with this
    /// node's key, as a build before D99 could have left it. Returns it as it would travel.
    fn grow_last(&self, kind: &str, field: &str, bytes: usize) -> WireEvent {
        let c = self.sql();
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
                 WHERE kind = ?1 AND node_id = (SELECT node_id FROM nodes WHERE is_local = 1)
                 ORDER BY seq DESC LIMIT 1",
                [kind],
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
        p[field] = json!("d".repeat(bytes));
        let payload = p.to_string();
        let key = crypto::decode_secret(&self.key()).unwrap();
        let sig = crypto::sign(
            &key,
            &crypto::event_message(&event_id, &node_id, ts, &actor, kind, &payload),
        );
        c.execute(
            "UPDATE events SET payload = ?2, sig = ?3 WHERE event_id = ?1",
            params![event_id, payload, sig],
        )
        .unwrap();
        WireEvent {
            seq,
            event_id,
            node_id,
            ts,
            actor,
            kind: kind.into(),
            payload,
            prev_id,
            sig,
        }
    }

    /// The payloads of the `kind` events of `origin` this node stores, oldest first.
    fn stored(&self, kind: &str, origin: &str) -> Vec<Value> {
        let c = self.sql();
        let mut stmt = c
            .prepare("SELECT payload FROM events WHERE kind = ?1 AND node_id = ?2 ORDER BY seq")
            .unwrap();
        let rows = stmt
            .query_map([kind, origin], |r| r.get::<_, String>(0))
            .unwrap();
        rows.map(|p| serde_json::from_str(&p.unwrap()).unwrap())
            .collect()
    }

    /// The event ids the stored `kind` events of `origin` name.
    fn named(&self, kind: &str, origin: &str) -> Vec<String> {
        self.stored(kind, origin)
            .iter()
            .map(|p| p["event_id"].as_str().unwrap().to_string())
            .collect()
    }

    fn holds_event(&self, event_id: &str) -> bool {
        self.count(&format!(
            "SELECT count(*) FROM events WHERE event_id = '{event_id}'"
        )) == 1
    }
}

fn pair_with(hub: &Node, spoke: &Node, perms: &[Perm]) {
    let code = hub
        .store
        .create_invite("https://hub.example", perms, 3600)
        .unwrap();
    let (invite, req) = spoke.store.prepare_join(&code).unwrap();
    let resp = hub.store.accept_pairing(&req).unwrap();
    spoke.store.complete_join(&invite, &resp).unwrap();
}

fn pair(hub: &Node, spoke: &Node) {
    pair_with(hub, spoke, &Perm::ALL);
}

fn add(total: &mut IngestReport, r: IngestReport) {
    total.accepted += r.accepted;
    total.duplicates += r.duplicates;
    total.rejected.extend(r.rejected);
}

/// The push half of `another sync`: batch after batch until nothing is left, each with the
/// markers of the events its scan stepped past written as the cursor moves.
fn push(hub: &Node, spoke: &Node) -> IngestReport {
    let hub_id = hub.id();
    let mut total = IngestReport::default();
    loop {
        let (cursor, _) = spoke.store.push_cursor(&hub_id).unwrap();
        let (events, high, skipped) = spoke
            .store
            .outbound_events_within(cursor, 500, PUSH_BATCH_BUDGET_BYTES)
            .unwrap();
        if high == cursor {
            break;
        }
        let last = events.last().map(|e| e.event_id.clone());
        add(
            &mut total,
            hub.store.ingest(&spoke.id(), &events, &[]).unwrap(),
        );
        spoke
            .store
            .advance_push_past(&hub_id, high, last.as_deref(), &skipped)
            .unwrap();
    }
    total
}

/// The pull half of `another sync`: page after page until the hub has no more.
fn pull(hub: &Node, spoke: &Node) -> IngestReport {
    let hub_id = hub.id();
    let mut total = IngestReport::default();
    loop {
        let (_, cursor) = spoke.store.push_cursor(&hub_id).unwrap();
        let page = hub
            .store
            .pull_for_from(&spoke.id(), cursor, &PullAnchors::default(), 500)
            .unwrap();
        add(
            &mut total,
            spoke
                .store
                .ingest(&hub_id, &page.events, &page.nodes)
                .unwrap(),
        );
        let last = page.events.last().map(|e| (e.event_id.as_str(), e.seq));
        spoke
            .store
            .advance_pull(&hub_id, page.cursor, last)
            .unwrap();
        if !page.more || page.cursor == cursor {
            break;
        }
    }
    total
}

fn sync(hub: &Node, spoke: &Node) {
    let pushed = push(hub, spoke);
    assert!(pushed.rejected.is_empty(), "{:?}", pushed.rejected);
    let pulled = pull(hub, spoke);
    assert!(pulled.rejected.is_empty(), "{:?}", pulled.rejected);
}

/// A marker payload as a holder writes it.
fn marker(event_id: &str, event_node: &str) -> Value {
    json!({
        "event_id": event_id,
        "event_node": event_node,
        "event_kind": "agent.registered",
        "event_actor": "system",
        "bytes": 9_437_500,
        "direction": "push",
        "about": "agent wordy",
        "route": "directory",
        "route_nodes": [],
    })
}

fn invalid_reason(r: &IngestReport, kind: &str) -> bool {
    r.accepted == 0
        && r.rejected.len() == 1
        && r.rejected[0].reason.starts_with(&format!("{kind} needs "))
}

/// 4.4: a marker goes where its event would have gone, and so does its acknowledgement.
/// Three spokes receive: `feed` has `read_feed`, `plain` has not, `host` has not and hosts
/// the recipient of the skipped message. The directory event's marker reaches all three,
/// the hold's marker only the spoke with `read_feed`, the message's marker only the
/// recipient's node.
#[test]
fn a_marker_travels_where_its_event_would_have() {
    let hub = node("hub");
    let a = node("spokea");
    let feed = node("feed");
    let plain = node("plain");
    let host = node("host");
    pair(&hub, &a);
    pair(&hub, &feed);
    pair_with(&hub, &plain, &[Perm::SendMail]);
    pair_with(&hub, &host, &[Perm::SendMail]);
    host.register("rita");
    a.register("alice");
    for spoke in [&host, &a, &feed, &plain] {
        sync(&hub, spoke);
    }
    sync(&hub, &a);

    a.register("wordy");
    let directory = a.grow_last("agent.registered", "description", 9 * MIB);
    a.store
        .acquire_holds(AcquireHolds {
            agent: "alice".into(),
            project: a.project("app"),
            paths: vec!["src/lib.rs".into()],
            ttl_seconds: None,
            exclusive: true,
            reason: None,
            repo_wide: false,
        })
        .unwrap();
    let hold = a.grow_last("hold.acquired", "reason", 9 * MIB);
    a.store
        .send_message(SendMessage {
            from: "alice".into(),
            to: vec!["rita@@host".into()],
            subject: "hello".into(),
            body: "small".into(),
            ..Default::default()
        })
        .unwrap();
    let mail = a.grow_last("message.sent", "pad", 9 * MIB);
    a.register("after");

    sync(&hub, &a);
    let a_id = a.id();
    let own = a.stored("sync.skipped", &a_id);
    assert_eq!(own.len(), 3);
    let routes: Vec<(String, String, Vec<String>)> = own
        .iter()
        .map(|p| {
            (
                p["event_id"].as_str().unwrap().to_string(),
                p["route"].as_str().unwrap().to_string(),
                p["route_nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|n| n.as_str().unwrap().to_string())
                    .collect(),
            )
        })
        .collect();
    assert_eq!(
        routes,
        [
            (directory.event_id.clone(), "directory".to_string(), vec![]),
            (hold.event_id.clone(), "feed".to_string(), vec![]),
            (mail.event_id.clone(), "mail".to_string(), vec![host.id()]),
        ]
    );
    // The hub holds all three markers and none of the events.
    assert_eq!(hub.named("sync.skipped", &a_id).len(), 3);
    for e in [&directory, &hold, &mail] {
        assert!(!hub.holds_event(&e.event_id));
    }
    assert!(hub.store.agent("after@@spokea").is_ok());

    let everywhere = |kind: &str| -> [Vec<String>; 3] {
        [&feed, &plain, &host].map(|spoke| {
            sync(&hub, spoke);
            let mut ids = spoke.named(kind, &a_id);
            ids.sort();
            ids
        })
    };
    let sorted = |mut ids: Vec<String>| {
        ids.sort();
        ids
    };
    let want = [
        sorted(vec![directory.event_id.clone(), hold.event_id.clone()]),
        vec![directory.event_id.clone()],
        sorted(vec![directory.event_id.clone(), mail.event_id.clone()]),
    ];
    assert_eq!(everywhere("sync.skipped"), want);

    // Each acknowledgement reaches the same spokes as its marker.
    for e in [&directory, &hold, &mail] {
        assert!(matches!(
            a.store.acknowledge_skip(&e.event_id).unwrap(),
            SkipAck::Skip { .. }
        ));
    }
    sync(&hub, &a);
    assert_eq!(everywhere("sync.skip_acked"), want);
}

/// R8: a spoke's marker that names another node's event is refused, by the hub and by a
/// spoke it is handed to; a spoke's marker for its own event is accepted.
#[test]
fn a_marker_for_another_nodes_event_is_refused() {
    let hub = node("hub");
    let a = node("spokea");
    let b = node("spokeb");
    let c = node("spokec");
    for spoke in [&a, &b, &c] {
        pair(&hub, spoke);
    }
    for spoke in [&a, &b, &c] {
        sync(&hub, spoke);
    }
    sync(&hub, &b);
    let c_id = c.id();
    let reason = format!(
        "sync.skipped names an event of node {c_id}; a node reports its own events, and only \
         this node's hub those of others"
    );

    let forged = a.signed("sync.skipped", &marker("01EVENTOFC", &c_id));
    let r = hub
        .store
        .ingest(&a.id(), std::slice::from_ref(&forged), &[])
        .unwrap();
    assert_eq!((r.accepted, r.rejected.len()), (0, 1));
    assert_eq!(r.rejected[0].reason, reason);
    assert!(!hub.holds_event(&forged.event_id));

    // The same event handed to a spoke as if the hub relayed it.
    let r = b
        .store
        .ingest(&hub.id(), std::slice::from_ref(&forged), &[])
        .unwrap();
    assert_eq!((r.accepted, r.rejected.len()), (0, 1));
    assert_eq!(r.rejected[0].reason, reason);
    assert!(!b.holds_event(&forged.event_id));

    // A node id is shown cut: nothing a peer chose reaches the reason whole.
    let long = a.signed("sync.skipped", &marker("01EVENT", &"N".repeat(128)));
    let r = hub.store.ingest(&a.id(), &[long], &[]).unwrap();
    assert_eq!(
        r.rejected[0].reason,
        format!(
            "sync.skipped names an event of node {}…; a node reports its own events, and only \
             this node's hub those of others",
            "N".repeat(39)
        )
    );

    let own = a.signed("sync.skipped", &marker("01EVENTOFA", &a.id()));
    let r = hub
        .store
        .ingest(&a.id(), std::slice::from_ref(&own), &[])
        .unwrap();
    assert_eq!((r.accepted, r.rejected.len()), (1, 0), "{:?}", r.rejected);
    assert!(hub.holds_event(&own.event_id));
    let r = b
        .store
        .ingest(&hub.id(), std::slice::from_ref(&own), &[])
        .unwrap();
    assert_eq!((r.accepted, r.rejected.len()), (1, 0), "{:?}", r.rejected);
}

/// R8: a marker that names another node's event is taken only from this node's own hub.
#[test]
fn a_hubs_marker_is_taken_only_from_this_nodes_hub() {
    let hub = node("hub");
    let a = node("spokea");
    let b = node("spokeb");
    let c = node("spokec");
    for spoke in [&a, &b, &c] {
        pair(&hub, spoke);
    }
    for spoke in [&a, &b, &c] {
        sync(&hub, spoke);
    }
    sync(&hub, &b);
    let payload = marker("01EVENTOFC", &c.id());

    let by_hub = hub.signed("sync.skipped", &payload);
    let r = b
        .store
        .ingest(&hub.id(), std::slice::from_ref(&by_hub), &[])
        .unwrap();
    assert_eq!((r.accepted, r.rejected.len()), (1, 0), "{:?}", r.rejected);
    assert!(b.holds_event(&by_hub.event_id));

    let by_spoke = a.signed("sync.skipped", &payload);
    let r = b
        .store
        .ingest(&hub.id(), std::slice::from_ref(&by_spoke), &[])
        .unwrap();
    assert_eq!((r.accepted, r.rejected.len()), (0, 1));
    assert!(r.rejected[0]
        .reason
        .starts_with("sync.skipped names an event of node "));
    assert!(!b.holds_event(&by_spoke.event_id));

    // Once this node revokes its hub, the hub's word about other nodes counts no more.
    b.store.revoke_peer(&hub.id()).unwrap();
    assert!(b.store.unacknowledged_skips().unwrap().is_empty());
}

/// R8, R9: a marker or an acknowledgement with a field missing, of the wrong type or over
/// its length is refused as invalid and not stored; at its length it is accepted; control
/// and text-direction characters within the length are accepted and stored as sent.
#[test]
fn a_marker_is_refused_for_shape_and_length_and_accepted_with_unsafe_characters() {
    let hub = node("hub");
    let a = node("spokea");
    pair(&hub, &a);
    sync(&hub, &a);
    let a_id = a.id();
    let base = marker("01EVENT", &a_id);
    let push_one = |kind: &str, payload: &Value| {
        let ev = a.signed(kind, payload);
        let r = hub
            .store
            .ingest(&a_id, std::slice::from_ref(&ev), &[])
            .unwrap();
        let stored = hub.holds_event(&ev.event_id);
        (r, stored, ev)
    };
    let with = |key: &str, value: Value| {
        let mut p = base.clone();
        p[key] = value;
        p
    };
    let without = |key: &str| {
        let mut p = base.clone();
        p.as_object_mut().unwrap().remove(key);
        p
    };
    let many = |n: usize, chars: usize| -> Value {
        json!((0..n).map(|_| "N".repeat(chars)).collect::<Vec<_>>())
    };

    let mut refused: Vec<(String, Value)> = Vec::new();
    for key in [
        "event_id",
        "event_node",
        "event_kind",
        "event_actor",
        "bytes",
        "direction",
        "route",
        "route_nodes",
    ] {
        refused.push((format!("{key} missing"), without(key)));
    }
    for key in [
        "event_id",
        "event_node",
        "event_kind",
        "event_actor",
        "direction",
        "about",
        "route",
    ] {
        refused.push((format!("{key} a number"), with(key, json!(7))));
    }
    refused.extend([
        ("bytes as text".to_string(), with("bytes", json!("9"))),
        ("bytes negative".to_string(), with("bytes", json!(-1))),
        ("bytes a fraction".to_string(), with("bytes", json!(1.5))),
        ("about null".to_string(), with("about", Value::Null)),
        (
            "route_nodes as text".to_string(),
            with("route_nodes", json!("x")),
        ),
        (
            "route_nodes of numbers".to_string(),
            with("route_nodes", json!([1, 2])),
        ),
        ("event_id empty".to_string(), with("event_id", json!(""))),
        (
            "event_node empty".to_string(),
            with("event_node", json!("")),
        ),
        (
            "event_kind empty".to_string(),
            with("event_kind", json!("")),
        ),
        (
            "event_id of 129".to_string(),
            with("event_id", json!("é".repeat(129))),
        ),
        (
            "event_node of 129".to_string(),
            with("event_node", json!("N".repeat(129))),
        ),
        (
            "event_kind of 81".to_string(),
            with("event_kind", json!("k".repeat(81))),
        ),
        (
            "event_actor of 129".to_string(),
            with("event_actor", json!("a".repeat(129))),
        ),
        (
            "about of 121".to_string(),
            with("about", json!("é".repeat(121))),
        ),
        (
            "direction sideways".to_string(),
            with("direction", json!("sideways")),
        ),
        (
            "route everywhere".to_string(),
            with("route", json!("everywhere")),
        ),
        (
            "1001 route_nodes".to_string(),
            with("route_nodes", many(1001, 1)),
        ),
        (
            "a route node of 129".to_string(),
            with("route_nodes", many(1, 129)),
        ),
    ]);
    for (what, payload) in &refused {
        let (r, stored, _) = push_one("sync.skipped", payload);
        assert!(invalid_reason(&r, "sync.skipped"), "{what}: {r:?}");
        assert!(!stored, "{what}");
    }

    // Each field exactly at its length, counted in characters, not bytes. `about` may be
    // absent. The node field is the origin's own id for a spoke.
    let mut at_bound = base.clone();
    at_bound["event_id"] = json!("é".repeat(128));
    at_bound["event_kind"] = json!("é".repeat(80));
    at_bound["event_actor"] = json!("é".repeat(128));
    at_bound["about"] = json!("é".repeat(120));
    at_bound["bytes"] = json!(0);
    at_bound["direction"] = json!("pull");
    at_bound["route"] = json!("mail");
    at_bound["route_nodes"] = many(1000, 128);
    for (what, payload) in [
        ("at every length", at_bound),
        ("without about", without("about")),
        ("with an empty actor", with("event_actor", json!(""))),
    ] {
        let (r, stored, _) = push_one("sync.skipped", &payload);
        assert_eq!(
            (r.accepted, r.rejected.len()),
            (1, 0),
            "{what}: {:?}",
            r.rejected
        );
        assert!(stored, "{what}");
    }
    // A node field at its length passes only from the receiver's hub.
    let by_hub = hub.signed("sync.skipped", &with("event_node", json!("N".repeat(128))));
    let r = a.store.ingest(&hub.id(), &[by_hub], &[]).unwrap();
    assert_eq!((r.accepted, r.rejected.len()), (1, 0), "{:?}", r.rejected);

    // Control and text-direction characters inside the lengths: accepted, stored as sent.
    let mut unsafe_text = base.clone();
    unsafe_text["event_id"] = json!("01\u{1b}]0;x\u{7}EVENT");
    unsafe_text["event_kind"] = json!("agent\u{202e}.registered\n");
    unsafe_text["event_actor"] = json!("al\u{0}ice\u{2066}");
    unsafe_text["about"] = json!("agent \u{1b}[2Jwordy\r\n");
    let (r, stored, ev) = push_one("sync.skipped", &unsafe_text);
    assert_eq!((r.accepted, r.rejected.len()), (1, 0), "{:?}", r.rejected);
    assert!(stored);
    let kept: String = hub
        .sql()
        .query_row(
            "SELECT payload FROM events WHERE event_id = ?1",
            [&ev.event_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept, ev.payload);

    // The acknowledgement: the same bounds on its three fields.
    let ack = json!({ "event_id": "01EVENT", "route": "feed", "route_nodes": [] });
    let ack_with = |key: &str, value: Value| {
        let mut p = ack.clone();
        p[key] = value;
        p
    };
    let ack_without = |key: &str| {
        let mut p = ack.clone();
        p.as_object_mut().unwrap().remove(key);
        p
    };
    for (what, payload) in [
        ("event_id missing", ack_without("event_id")),
        ("route missing", ack_without("route")),
        ("route_nodes missing", ack_without("route_nodes")),
        ("event_id a number", ack_with("event_id", json!(1))),
        ("event_id empty", ack_with("event_id", json!(""))),
        (
            "event_id of 129",
            ack_with("event_id", json!("é".repeat(129))),
        ),
        ("route a number", ack_with("route", json!(1))),
        ("route everywhere", ack_with("route", json!("everywhere"))),
        ("route_nodes as text", ack_with("route_nodes", json!("x"))),
        ("1001 route_nodes", ack_with("route_nodes", many(1001, 1))),
        ("a route node of 129", ack_with("route_nodes", many(1, 129))),
    ] {
        let (r, stored, _) = push_one("sync.skip_acked", &payload);
        assert!(invalid_reason(&r, "sync.skip_acked"), "{what}: {r:?}");
        assert!(!stored, "{what}");
    }
    let mut ack_at_bound = ack.clone();
    ack_at_bound["event_id"] = json!("é".repeat(128));
    ack_at_bound["route"] = json!("mail");
    ack_at_bound["route_nodes"] = many(1000, 128);
    for (what, payload) in [
        ("at every length", ack_at_bound),
        (
            "with unsafe characters",
            ack_with("event_id", json!("01\u{1b}[2J\u{202e}EVENT")),
        ),
    ] {
        let (r, stored, ev) = push_one("sync.skip_acked", &payload);
        assert_eq!(
            (r.accepted, r.rejected.len()),
            (1, 0),
            "{what}: {:?}",
            r.rejected
        );
        assert!(stored, "{what}");
        let kept: String = hub
            .sql()
            .query_row(
                "SELECT payload FROM events WHERE event_id = ?1",
                [&ev.event_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, ev.payload, "{what}");
    }
}

/// R9: an acknowledgement counts for a marker only when both come from the same node. A
/// spoke's acknowledgement that names its hub's skipped event silences nothing; the hub's
/// own does.
#[test]
fn an_acknowledgement_from_another_node_silences_nothing() {
    let hub = node("hub");
    let a = node("spokea");
    let s = node("spokes");
    pair(&hub, &a);
    pair(&hub, &s);
    sync(&hub, &a);
    sync(&hub, &s);
    hub.register("wordy");
    let skipped = hub.grow_last("agent.registered", "description", 33 * MIB);
    sync(&hub, &s);
    let reported = |n: &Node| -> Vec<String> {
        n.store
            .unacknowledged_skips()
            .unwrap()
            .into_iter()
            .map(|skip| skip.event_id)
            .collect()
    };
    assert_eq!(reported(&s), std::slice::from_ref(&skipped.event_id));
    assert_eq!(reported(&hub), std::slice::from_ref(&skipped.event_id));
    assert!(!s.holds_event(&skipped.event_id));

    let forged = a.signed(
        "sync.skip_acked",
        &json!({ "event_id": skipped.event_id, "route": "directory", "route_nodes": [] }),
    );
    let r = hub
        .store
        .ingest(&a.id(), std::slice::from_ref(&forged), &[])
        .unwrap();
    assert_eq!((r.accepted, r.rejected.len()), (1, 0), "{:?}", r.rejected);
    sync(&hub, &s);
    assert!(s.holds_event(&forged.event_id));
    assert_eq!(reported(&s), std::slice::from_ref(&skipped.event_id));
    assert_eq!(reported(&hub), std::slice::from_ref(&skipped.event_id));

    // The holder's own acknowledgement does.
    hub.store.acknowledge_skip(&skipped.event_id).unwrap();
    assert!(reported(&hub).is_empty());
    assert_eq!(reported(&s), std::slice::from_ref(&skipped.event_id));
    sync(&hub, &s);
    assert!(reported(&s).is_empty());
}

fn dump(n: &Node, table: &str) -> Vec<String> {
    let c = n.sql();
    let mut stmt = c
        .prepare(&format!("SELECT * FROM {table} ORDER BY 1"))
        .unwrap();
    let columns = stmt.column_count();
    let rows = stmt
        .query_map([], |r| {
            let mut out = Vec::new();
            for i in 0..columns {
                out.push(format!("{:?}", r.get::<_, rusqlite::types::Value>(i)?));
            }
            Ok(out.join("|"))
        })
        .unwrap();
    rows.map(|r| r.unwrap()).collect()
}

/// R10: neither kind has a projection. The read models are the same before and after a
/// marker and an acknowledgement arrive.
#[test]
fn a_marker_changes_no_read_model() {
    let hub = node("hub");
    let a = node("spokea");
    pair(&hub, &a);
    hub.register("bob");
    let alice = a.register("alice");
    a.store
        .acquire_holds(AcquireHolds {
            agent: "alice".into(),
            project: a.project("app"),
            paths: vec!["src/lib.rs".into()],
            ttl_seconds: None,
            exclusive: true,
            reason: Some("editing".into()),
            repo_wide: false,
        })
        .unwrap();
    a.store
        .register_resource(RegisterResource {
            agent: "alice".into(),
            kind: ResourceKind::Port,
            name: "tcp:127.0.0.1:5999".into(),
            endpoint: None,
            port: None,
            pid: None,
            project: None,
            purpose: None,
            mode: LeaseMode::Shared,
            ttl_seconds: None,
            metadata: None,
        })
        .unwrap();
    sync(&hub, &a);
    a.store
        .send_message(SendMessage {
            from: "alice".into(),
            to: vec!["bob@@hub".into()],
            subject: "hello".into(),
            body: "small".into(),
            ..Default::default()
        })
        .unwrap();
    sync(&hub, &a);

    let tables = [
        "agents",
        "holds",
        "messages",
        "resources",
        "resource_leases",
    ];
    let before: Vec<Vec<String>> = tables.iter().map(|t| dump(&hub, t)).collect();
    assert!(before.iter().all(|rows| !rows.is_empty()));

    // The marker names real objects of the origin: its agent, as kind, actor and about.
    let mut payload = marker(&alice.agent_id, &a.id());
    payload["event_actor"] = json!("alice");
    let skipped = a.signed("sync.skipped", &payload);
    let acked = a.signed(
        "sync.skip_acked",
        &json!({ "event_id": alice.agent_id, "route": "directory", "route_nodes": [] }),
    );
    let r = hub.store.ingest(&a.id(), &[skipped, acked], &[]).unwrap();
    assert_eq!((r.accepted, r.rejected.len()), (2, 0), "{:?}", r.rejected);

    let after: Vec<Vec<String>> = tables.iter().map(|t| dump(&hub, t)).collect();
    assert_eq!(before, after);
}

/// 4.8: the remedy runs on the node that wrote the event and the acknowledgement on the
/// holder. A hub's marker for an event it relays sends the reader to the event's node.
#[test]
fn a_hubs_marker_for_a_relayed_event_sends_the_reader_to_the_events_node() {
    let hub = node("hub");
    let s = node("spokes");
    let t = node("spoket");
    pair(&hub, &s);
    pair(&hub, &t);
    for spoke in [&s, &t, &s] {
        sync(&hub, spoke);
    }
    // An event of spoke T that neither spoke holds, marked by the hub.
    let mut payload = marker("01RE1AYED00000000000000000", &t.id());
    payload["direction"] = json!("pull");
    let by_hub = hub.signed("sync.skipped", &payload);
    for spoke in [&s, &t] {
        let r = spoke
            .store
            .ingest(&hub.id(), std::slice::from_ref(&by_hub), &[])
            .unwrap();
        assert_eq!((r.accepted, r.rejected.len()), (1, 0), "{:?}", r.rejected);
    }

    let on_s = s.store.unacknowledged_skips().unwrap();
    assert_eq!(on_s.len(), 1);
    assert_eq!(
        (
            on_s[0].event_node_name.as_str(),
            on_s[0].holder_name.as_str()
        ),
        ("spoket", "hub")
    );
    let block = on_s[0].sync_block();
    let lines: Vec<&str> = block.lines().collect();
    assert_eq!(
        lines[0],
        "skipped 01RE1AYED00000000000000000 (agent.registered, 9437500 bytes, written by system): it is in node \
         hub's log and is larger than a spoke reads in one pull answer (33554432 bytes), so \
         node hub did not send it; the events after it were sent"
    );
    assert_eq!(lines[1], "  about: agent wordy");
    assert!(lines[2].starts_with("  missing on this node: what this registration changed: "));
    assert_eq!(lines[3], "  to get it across, on node spoket:");
    assert_eq!(
        lines.last().unwrap(),
        &"  then, on node hub: another sync ack 01RE1AYED00000000000000000 --owner"
    );
    let steps = on_s[0].needs_steps();
    assert!(steps[0]
        .starts_with("on node spoket: register the agent again: `another agent register wordy "));
    assert_eq!(steps[5], "on node spoket: another sync");
    assert_eq!(
        steps.last().unwrap(),
        "on node hub: another sync ack 01RE1AYED00000000000000000 --owner"
    );
    assert_eq!(
        on_s[0].needs_line(),
        "skipped event  01RE1AYED00000000000000000 (agent.registered, 9437500 bytes) in node hub's log was not \
         sent to this node: larger than one pull answer"
    );

    // On the node that wrote the event the steps run here; the acknowledgement stays the
    // hub's.
    let on_t = t.store.unacknowledged_skips().unwrap();
    assert_eq!(on_t.len(), 1);
    let block = on_t[0].sync_block();
    assert!(
        block.contains("\n  to get it across, on this node:\n"),
        "{block}"
    );
    assert!(block
        .ends_with("\n  then, on node hub: another sync ack 01RE1AYED00000000000000000 --owner"));
    let steps = on_t[0].needs_steps();
    assert!(steps[0].starts_with("register the agent again: `another agent register wordy "));
    assert_eq!(steps[5], "another sync");
    assert_eq!(
        steps.last().unwrap(),
        "on node hub: another sync ack 01RE1AYED00000000000000000 --owner"
    );
}

fn port_resource(
    n: &Node,
    agent: &str,
    name: &str,
    endpoint: Option<String>,
    mode: LeaseMode,
) -> String {
    n.store
        .register_resource(RegisterResource {
            agent: agent.into(),
            kind: ResourceKind::Port,
            name: name.into(),
            endpoint,
            port: None,
            pid: None,
            project: None,
            purpose: None,
            mode,
            ttl_seconds: Some(3600),
            metadata: None,
        })
        .unwrap()
        .resource
        .resource_id
}

/// 4.10 step 3: between the release and the borrow of an own resource it has no lease, so
/// another local agent that borrows in that moment becomes its owner. The first agent then
/// comes back as a borrower, or is refused when the resource is exclusive.
#[test]
fn an_own_resource_released_for_the_remedy_can_be_adopted_by_another_agent_in_between() {
    let n = node("solo");
    n.register("wordy");
    n.register("other");
    for (name, mode) in [
        ("tcp:127.0.0.1:6101", LeaseMode::Shared),
        ("tcp:127.0.0.1:6102", LeaseMode::Exclusive),
    ] {
        let rid = ResourceRef::Id(port_resource(&n, "wordy", name, None, mode));
        let released = n.store.release_resource("wordy", &rid, false).unwrap();
        assert_eq!(released.outcome, "released");
        assert_eq!(released.resource.refcount, 0);
        let taken = n.store.borrow_resource("other", &rid, None).unwrap();
        assert_eq!(taken.outcome, "adopted");
        assert_eq!(taken.lease.unwrap().role, "owner");
        let back = n.store.borrow_resource("wordy", &rid, None);
        match mode {
            LeaseMode::Shared => {
                let back = back.unwrap();
                assert_eq!(back.outcome, "borrowed");
                assert_eq!(back.lease.unwrap().role, "borrower");
            }
            LeaseMode::Exclusive => {
                let text = back.unwrap_err().to_string();
                assert_eq!(text, format!("port {name} is exclusive to other"));
            }
        }
    }
}

/// 4.10 step 4: a borrow that is refused after the release leaves the lease released. Here
/// the hub's resource can no longer be reached from the spoke's machine.
#[test]
fn a_borrow_refused_after_the_release_leaves_the_lease_released() {
    let hub = node("hub");
    let s = node("spokes");
    pair(&hub, &s);
    hub.register("owner");
    s.register("wordy");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let rid = port_resource(
        &hub,
        "owner",
        &format!("tcp:127.0.0.1:{port}"),
        Some(format!("http://127.0.0.1:{port}")),
        LeaseMode::Shared,
    );
    sync(&hub, &s);
    let rid = ResourceRef::Id(rid);
    let borrowed = s.store.borrow_resource("wordy", &rid, None).unwrap();
    assert_eq!(borrowed.outcome, "borrowed");

    drop(listener);
    let released = s.store.release_resource("wordy", &rid, false).unwrap();
    assert_eq!(released.outcome, "released");
    let refused = s
        .store
        .borrow_resource("wordy", &rid, None)
        .unwrap_err()
        .to_string();
    assert_eq!(
        refused,
        format!("port tcp:127.0.0.1:{port} on node hub is not reachable from this machine")
    );
    let wordy = s.store.agent("wordy").unwrap().agent_id;
    assert_eq!(
        s.count(&format!(
            "SELECT count(*) FROM resource_leases
             WHERE agent_id = '{wordy}' AND released_at IS NULL"
        )),
        0
    );
}

fn live_leases(n: &Node, resource_id: &str, agent_id: &str) -> i64 {
    let now = n.clock.load(Ordering::SeqCst);
    n.count(&format!(
        "SELECT count(*) FROM resource_leases
         WHERE resource_id = '{resource_id}' AND agent_id = '{agent_id}'
           AND released_at IS NULL AND expires_at > {now}"
    ))
}

/// 4.10, `resource.renewed`: a skipped renewal of a borrow on another node's resource does
/// not heal. That node ends the lease when it runs out there and says so, which ends it on
/// the borrower too, and the next renewal changes nothing; borrowing again by id restores
/// it. A skipped renewal of a lease on the holder's own resource heals with the next one.
#[test]
fn a_skipped_renewal_of_a_borrow_on_another_node_ends_it_and_borrowing_again_restores_it() {
    let hub = node("hub");
    let s = node("spokes");
    pair(&hub, &s);
    hub.register("owner");
    let bea = s.register("bea").agent_id;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let theirs = port_resource(
        &hub,
        "owner",
        &format!("tcp:127.0.0.1:{port}"),
        Some(format!("http://127.0.0.1:{port}")),
        LeaseMode::Shared,
    );
    let own = s
        .store
        .register_resource(RegisterResource {
            agent: "bea".into(),
            kind: ResourceKind::Port,
            name: "tcp:127.0.0.1:6201".into(),
            endpoint: None,
            port: None,
            pid: None,
            project: None,
            purpose: None,
            mode: LeaseMode::Shared,
            ttl_seconds: Some(60),
            metadata: None,
        })
        .unwrap()
        .resource
        .resource_id;
    sync(&hub, &s);
    let theirs_ref = ResourceRef::Id(theirs.clone());
    let borrowed = s
        .store
        .borrow_resource("bea", &theirs_ref, Some(60))
        .unwrap();
    assert_eq!(borrowed.outcome, "borrowed");
    sync(&hub, &s);
    assert_eq!(live_leases(&hub, &theirs, &bea), 1);
    assert_eq!(live_leases(&hub, &own, &bea), 1);

    // At 30 s both leases are renewed in one event, and that event is too large to send.
    for n in [&hub, &s] {
        n.clock.store(T0 + 30_000, Ordering::SeqCst);
    }
    s.store.heartbeat_resources("bea", &[]).unwrap();
    let renewal = s.grow_last("resource.renewed", "pad", 9 * MIB);
    sync(&hub, &s);
    assert_eq!(
        s.named("sync.skipped", &s.id()),
        std::slice::from_ref(&renewal.event_id)
    );
    assert!(!hub.holds_event(&renewal.event_id));
    assert_eq!(live_leases(&s, &theirs, &bea), 1);

    // At 70 s the owner's node has seen no renewal: its sweep ends the borrow.
    for n in [&hub, &s] {
        n.clock.store(T0 + 70_000, Ordering::SeqCst);
    }
    hub.store.sweep().unwrap();
    assert_eq!(live_leases(&hub, &theirs, &bea), 0);
    assert_eq!(live_leases(&hub, &own, &bea), 0);
    sync(&hub, &s);
    assert_eq!(live_leases(&s, &theirs, &bea), 0);
    // The next renewal brings the borrow back nowhere, and heals the own lease.
    s.store.heartbeat_resources("bea", &[]).unwrap();
    sync(&hub, &s);
    assert_eq!(live_leases(&s, &theirs, &bea), 0);
    assert_eq!(live_leases(&hub, &theirs, &bea), 0);
    assert_eq!(live_leases(&s, &own, &bea), 1);
    assert_eq!(live_leases(&hub, &own, &bea), 1);

    // The remedy: borrow again, by resource id.
    let again = s
        .store
        .borrow_resource("bea", &theirs_ref, Some(60))
        .unwrap();
    assert_eq!(again.outcome, "borrowed");
    sync(&hub, &s);
    assert_eq!(live_leases(&hub, &theirs, &bea), 1);
    drop(listener);
}

/// A log can hold a `sync.skipped` or `sync.skip_acked` of any shape: an earlier build
/// stored these kinds unchecked, and any local process can write the database. Such rows
/// match nothing, and the report and the acknowledgement still work around them.
#[test]
fn a_marker_of_any_shape_in_the_log_does_not_break_the_report() {
    let n = node("solo");
    let n_id = n.id();
    let c = n.sql();
    for (i, (kind, payload)) in [
        ("sync.skipped", "{"),
        ("sync.skipped", "\"text\""),
        ("sync.skipped", "[1, 2]"),
        ("sync.skipped", "{\"event_id\": 7}"),
        ("sync.skipped", "{\"event_id\": null, \"bytes\": \"many\"}"),
        ("sync.skip_acked", "not json"),
        ("sync.skip_acked", "{\"event_id\": 7}"),
        ("sync.skip_acked", "{\"event_id\": [\"01REA1\"]}"),
    ]
    .iter()
    .enumerate()
    {
        c.execute(
            "INSERT INTO events (event_id, node_id, ts, actor, kind, payload, prev_id, sig)
             VALUES (?1, ?2, ?3, 'system', ?4, ?5, NULL, 'none')",
            params![format!("01ODD{i}"), n_id, T0, kind, payload],
        )
        .unwrap();
    }
    assert!(n.store.unacknowledged_skips().unwrap().is_empty());
    assert!(matches!(
        n.store.skip_to_acknowledge("7"),
        Err(Error::NotFound(_))
    ));

    // A real skip beside them is reported, and acknowledged, as usual.
    n.store
        .mark_pull_skips(&[SkippedEvent {
            event_id: "01REA1".into(),
            event_node: n_id.clone(),
            event_kind: "agent.registered".into(),
            event_actor: "wordy".into(),
            bytes: 34_000_000,
            about: Some("agent wordy".into()),
            route: "directory".into(),
            route_nodes: Vec::new(),
        }])
        .unwrap();
    let reported = n.store.unacknowledged_skips().unwrap();
    assert_eq!(reported.len(), 1);
    assert_eq!(reported[0].event_id, "01REA1");
    assert!(reported[0].own && reported[0].event_is_local);
    assert_eq!(reported[0].direction, "pull");
    assert!(matches!(
        n.store.acknowledge_skip("01REA1").unwrap(),
        SkipAck::Skip { .. }
    ));
    assert!(matches!(
        n.store.acknowledge_skip("01REA1").unwrap(),
        SkipAck::Already { .. }
    ));
    assert!(n.store.unacknowledged_skips().unwrap().is_empty());
    assert_eq!(
        n.count("SELECT count(*) FROM events WHERE kind = 'sync.skip_acked' AND actor = 'human (another sync ack)'"),
        1
    );
}
