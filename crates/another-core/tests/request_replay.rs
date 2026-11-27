//! P3-1: a signed node-to-node request is served once, and by the hub it was signed for
//! only. Each test plays the attacker against `Store::verify_and_consume_peer_request`: it
//! holds a request the spoke really signed and tries to have the hub accept it again.

use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use another_core::*;
use rusqlite::{params, Connection};

const START: i64 = 1_800_000_000_000;
const MINUTE: i64 = 60 * 1000;

struct Hub {
    store: Store,
    home: std::path::PathBuf,
    clock: Arc<AtomicI64>,
}

impl Hub {
    fn new(dir: &Path, name: &str) -> Hub {
        let home = dir.join(name);
        let (clock, handle) = Clock::manual(START);
        let store = Store::open_with_clock(&home, clock).unwrap();
        store.set_node_name(name).unwrap();
        Hub {
            store,
            home,
            clock: handle,
        }
    }

    fn id(&self) -> String {
        self.store.node_id().to_string()
    }

    fn db(&self) -> Connection {
        Connection::open(self.home.join(DB_FILE)).unwrap()
    }

    fn set_time(&self, ms: i64) {
        self.clock.store(ms, Ordering::SeqCst);
    }

    fn kept(&self, node: &str) -> i64 {
        self.db()
            .query_row(
                "SELECT COUNT(*) FROM fed_request_nonces WHERE node_id = ?1",
                [node],
                |r| r.get(0),
            )
            .unwrap()
    }
}

/// A spoke whose clock reads `START`: it pairs at the hubs' time. Each request states its
/// own time.
fn spoke(dir: &Path, name: &str) -> Store {
    let (clock, _) = Clock::manual(START);
    let store = Store::open_with_clock(&dir.join(name), clock).unwrap();
    store.set_node_name(name).unwrap();
    store
}

fn pair(hub: &Hub, spoke: &Store) {
    let code = hub
        .store
        .create_invite("https://hub.example", &Perm::ALL, 3600)
        .unwrap();
    let (invite, req) = spoke.prepare_join(&code).unwrap();
    let resp = hub.store.accept_pairing(&req).unwrap();
    spoke.complete_join(&invite, &resp).unwrap();
}

/// One request as a spoke of this build signs it, for `hub_id`.
#[derive(Clone)]
struct Signed {
    node: String,
    method: &'static str,
    path: String,
    time: i64,
    nonce: String,
    body: Vec<u8>,
    sig: String,
}

fn sign(spoke: &Store, hub_id: &str, time: i64) -> Signed {
    let nonce = crypto::request_nonce().unwrap();
    let body = br#"{"events":[]}"#.to_vec();
    let path = "/fed/push".to_string();
    let sig = spoke.sign_request_v2(hub_id, "POST", &path, time, &nonce, &body);
    Signed {
        node: spoke.node_id().to_string(),
        method: "POST",
        path,
        time,
        nonce,
        body,
        sig,
    }
}

fn submit(hub: &Store, r: &Signed) -> Result<Vec<Perm>> {
    hub.verify_and_consume_peer_request(
        &r.node, r.method, &r.path, r.time, &r.nonce, &r.body, &r.sig,
    )
}

fn refused(result: Result<Vec<Perm>>, fragment: &str) {
    match result {
        Ok(_) => panic!("the hub accepted the request; expected {fragment:?}"),
        Err(e) => assert!(
            e.to_string().contains(fragment),
            "expected {fragment:?} in: {e}"
        ),
    }
}

const REPLAYED: &str = "request already seen";
const NOT_SIGNED: &str = "request not signed by a paired node";

#[test]
fn a_captured_request_is_served_once() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::new(dir.path(), "hub");
    let vic = spoke(dir.path(), "vic");
    pair(&hub, &vic);
    let captured = sign(&vic, &hub.id(), START);
    submit(&hub.store, &captured).expect("the honest request");
    for _ in 0..3 {
        refused(submit(&hub.store, &captured), REPLAYED);
    }
    // Later in its window too.
    hub.set_time(START + 4 * MINUTE);
    refused(submit(&hub.store, &captured), REPLAYED);
    assert_eq!(hub.kept(vic.node_id()), 1);
}

#[test]
fn a_captured_request_with_another_nonce_or_any_other_change_does_not_verify() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::new(dir.path(), "hub");
    let vic = spoke(dir.path(), "vic");
    pair(&hub, &vic);
    let captured = sign(&vic, &hub.id(), START);
    submit(&hub.store, &captured).unwrap();
    type Edit = Box<dyn Fn(&mut Signed)>;
    let edits: Vec<(&str, Edit)> = vec![
        (
            "nonce",
            Box::new(|r| r.nonce = crypto::request_nonce().unwrap()),
        ),
        ("method", Box::new(|r| r.method = "GET")),
        (
            "path",
            Box::new(|r| r.path = "/fed/pull?after=0&limit=500".into()),
        ),
        ("time", Box::new(|r| r.time += 1)),
        ("body", Box::new(|r| r.body = b"{\"events\":[ ]}".to_vec())),
    ];
    for (what, edit) in edits {
        let mut r = captured.clone();
        edit(&mut r);
        match submit(&hub.store, &r) {
            Ok(_) => panic!("{what}: accepted"),
            Err(e) => assert!(e.to_string().contains(NOT_SIGNED), "{what}: {e}"),
        }
    }
    // A refused request uses up nothing.
    assert_eq!(hub.kept(vic.node_id()), 1);
}

#[test]
fn the_older_signature_is_not_accepted_as_the_version_2_one() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::new(dir.path(), "hub");
    let vic = spoke(dir.path(), "vic");
    pair(&hub, &vic);
    let mut r = sign(&vic, &hub.id(), START);
    r.sig = vic.sign_request(r.method, &r.path, r.time, &r.body);
    refused(submit(&hub.store, &r), NOT_SIGNED);
}

#[test]
fn a_request_signed_for_one_hub_is_refused_by_another_that_pins_the_same_key() {
    let dir = tempfile::tempdir().unwrap();
    let first = Hub::new(dir.path(), "first");
    let second = Hub::new(dir.path(), "second");
    let vic = spoke(dir.path(), "vic");
    pair(&first, &vic);
    let captured = sign(&vic, &first.id(), START);
    submit(&first.store, &captured).unwrap();
    // The spoke moves to the second hub; the first one's request is still in its window.
    vic.revoke_peer(&first.id()).unwrap();
    pair(&second, &vic);
    refused(submit(&second.store, &captured), NOT_SIGNED);
    let fresh = sign(&vic, &second.id(), START);
    submit(&second.store, &fresh).expect("a request signed for the second hub");
}

#[test]
fn a_replay_after_the_hub_restarts_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::new(dir.path(), "hub");
    let vic = spoke(dir.path(), "vic");
    pair(&hub, &vic);
    let captured = sign(&vic, &hub.id(), START);
    submit(&hub.store, &captured).unwrap();
    let home = hub.home.clone();
    drop(hub);
    let (clock, _) = Clock::manual(START + MINUTE);
    let reopened = Store::open_with_clock(&home, clock).unwrap();
    refused(submit(&reopened, &captured), REPLAYED);
}

#[test]
fn a_replay_after_a_revoke_and_a_new_pairing_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::new(dir.path(), "hub");
    let vic = spoke(dir.path(), "vic");
    pair(&hub, &vic);
    let captured = sign(&vic, &hub.id(), START);
    submit(&hub.store, &captured).unwrap();
    hub.store.revoke_peer(vic.node_id()).unwrap();
    refused(submit(&hub.store, &captured), NOT_SIGNED);
    hub.store.readmit_peer(vic.node_id()).unwrap();
    refused(submit(&hub.store, &captured), REPLAYED);
    // Paired again from a new invite, it is the same node with the same key.
    hub.store.revoke_peer(vic.node_id()).unwrap();
    hub.store.readmit_peer(vic.node_id()).unwrap();
    pair(&hub, &vic);
    refused(submit(&hub.store, &captured), REPLAYED);
    submit(&hub.store, &sign(&vic, &hub.id(), START)).expect("a new request");
}

#[test]
fn concurrent_copies_of_one_request_are_served_once() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::new(dir.path(), "hub");
    let vic = spoke(dir.path(), "vic");
    pair(&hub, &vic);
    let captured = sign(&vic, &hub.id(), START);
    let accepted: usize = std::thread::scope(|s| {
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let store = hub.store.clone();
                let r = captured.clone();
                s.spawn(move || submit(&store, &r).is_ok())
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap() as usize)
            .sum()
    });
    assert_eq!(accepted, 1);
}

/// The nonce of an old request is deleted once it is more than ten minutes below the
/// newest accepted time. The floor then refuses it, even when the hub's clock has stepped
/// back far enough to put it inside the window again.
#[test]
fn a_pruned_request_stays_refused_when_the_hub_clock_steps_back() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::new(dir.path(), "hub");
    let vic = spoke(dir.path(), "vic");
    pair(&hub, &vic);
    let captured = sign(&vic, &hub.id(), START);
    submit(&hub.store, &captured).unwrap();
    hub.set_time(START + 11 * MINUTE);
    submit(&hub.store, &sign(&vic, &hub.id(), START + 11 * MINUTE)).unwrap();
    assert_eq!(hub.kept(vic.node_id()), 1, "the old nonce was pruned");
    hub.set_time(START);
    refused(
        submit(&hub.store, &captured),
        "older than requests this hub already accepted",
    );
}

#[test]
fn a_spoke_clock_that_steps_back_less_than_ten_minutes_is_still_served() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::new(dir.path(), "hub");
    let vic = spoke(dir.path(), "vic");
    pair(&hub, &vic);
    submit(&hub.store, &sign(&vic, &hub.id(), START + 4 * MINUTE)).unwrap();
    submit(&hub.store, &sign(&vic, &hub.id(), START - 4 * MINUTE)).expect("8 minutes back");
    assert_eq!(hub.kept(vic.node_id()), 2);
}

#[test]
fn a_hub_clock_that_jumps_ahead_moves_no_floor() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::new(dir.path(), "hub");
    let vic = spoke(dir.path(), "vic");
    pair(&hub, &vic);
    submit(&hub.store, &sign(&vic, &hub.id(), START)).unwrap();
    // A year ahead: the spoke's honest requests are out of the window, and nothing is kept.
    hub.set_time(START + 365 * 24 * 60 * MINUTE);
    refused(
        submit(&hub.store, &sign(&vic, &hub.id(), START + MINUTE)),
        "more than 5 minutes off",
    );
    hub.set_time(START + MINUTE);
    submit(&hub.store, &sign(&vic, &hub.id(), START + MINUTE)).expect("served again");
}

#[test]
fn the_nonces_kept_are_bounded_by_time_and_by_count() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::new(dir.path(), "hub");
    let vic = spoke(dir.path(), "vic");
    pair(&hub, &vic);
    // One request a minute for an hour: at most the last ten minutes' are kept.
    for minute in 0..60 {
        let t = START + minute * MINUTE;
        hub.set_time(t);
        submit(&hub.store, &sign(&vic, &hub.id(), t)).unwrap();
    }
    assert_eq!(hub.kept(vic.node_id()), 11);

    // At the count bound, a new request is refused and nothing is written.
    let now = START + 60 * MINUTE;
    let mut db = hub.db();
    let tx = db.transaction().unwrap();
    {
        let mut insert = tx
            .prepare(
                "INSERT INTO fed_request_nonces (node_id, nonce, request_time, seen_at)
                 VALUES (?1, ?2, ?3, ?3)",
            )
            .unwrap();
        for i in 11..REPLAY_ROWS_PER_NODE {
            insert
                .execute(params![vic.node_id(), format!("{i:032x}"), now])
                .unwrap();
        }
    }
    tx.commit().unwrap();
    assert_eq!(hub.kept(vic.node_id()), REPLAY_ROWS_PER_NODE);
    refused(
        submit(&hub.store, &sign(&vic, &hub.id(), now)),
        "too many signed requests",
    );
    assert_eq!(hub.kept(vic.node_id()), REPLAY_ROWS_PER_NODE);
}

#[test]
fn a_malformed_nonce_is_refused_before_anything_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::new(dir.path(), "hub");
    let vic = spoke(dir.path(), "vic");
    pair(&hub, &vic);
    for bad in ["", "xyz", "00112233445566778899AABBCCDDEEFF", "0011\n"] {
        let mut r = sign(&vic, &hub.id(), START);
        r.nonce = bad.to_string();
        r.sig = vic.sign_request_v2(&hub.id(), r.method, &r.path, r.time, bad, &r.body);
        refused(submit(&hub.store, &r), "x-another-nonce must be");
    }
    assert_eq!(hub.kept(vic.node_id()), 0);
}
