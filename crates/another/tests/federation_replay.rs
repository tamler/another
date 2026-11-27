//! P3-1: a push or pull that someone captured on the way to the hub cannot be sent again.
//! Each attack test lets a relay keep a spoke's real signed requests and then sends them to
//! the hub itself, as one who saw the traffic would. The mixed-version tests run a spoke or
//! a hub of the build before (`Exchange::SignedAnswersOnly`) against one of this build.

#[path = "common/relay.rs"]
mod relay;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use another::fed::{self, Exchange, Limits};
use another_core::*;
use relay::*;
use serde_json::Value;

const REPLAYED: &str = "request already seen: its nonce was used; a signed request is served once";
const NOT_SIGNED: &str = "request not signed by a paired node";
const N3: &str = "this hub serves only requests signed for it with their nonce \
                  (x-another-signature-v2), so a captured request cannot be sent again, and \
                  this request was not. Update this node to the hub's build, then run \
                  `another sync`. Nothing was stored, nothing is lost, and the node does not \
                  need to pair again";

/// Send `req` to `url` as it is, headers and body, and return the answer.
fn send(url: &str, req: &Seen) -> Answer {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .proxy(None)
            .timeout_global(Some(Duration::from_secs(30)))
            .build(),
    );
    let target = format!("{url}{}", req.path);
    let passed = |name: &str| name.starts_with("x-another-") || name == "content-type";
    let result = if req.method == "POST" {
        let mut r = agent.post(&target);
        for (n, v) in req.headers.iter().filter(|(n, _)| passed(n)) {
            r = r.header(n.as_str(), v.as_str());
        }
        r.send(&req.body[..])
    } else {
        let mut r = agent.get(&target);
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
        .map(|(n, v)| (n.as_str().to_string(), v.to_str().unwrap().to_string()))
        .collect();
    let body = resp.body_mut().read_to_vec().unwrap();
    Answer {
        status,
        headers,
        body,
    }
}

fn header<'a>(answer: &'a Answer, name: &str) -> Option<&'a str> {
    answer
        .headers
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

fn message(answer: &Answer) -> String {
    let v: Value = serde_json::from_slice(&answer.body).unwrap();
    v["error"]["message"].as_str().unwrap().to_string()
}

/// A hub of the build before replay-proof requests.
fn start_current_main_hub(name: &str) -> Hub {
    let node = node(name);
    let served = serve(fed::router_as(
        node.store.clone(),
        Limits::CURRENT,
        Exchange::SignedAnswersOnly,
    ));
    Hub { node, served }
}

fn current_main_sync(spoke: &Node) -> anyhow::Result<fed::SyncReport> {
    fed::sync_as(&spoke.store, &Limits::CURRENT, Exchange::SignedAnswersOnly)
}

/// `ok` rows the hub wrote for `operation`.
fn served(hub: &Hub, operation: &str) -> usize {
    audit_rows(&hub.node.store, operation)
        .iter()
        .filter(|a| a.outcome == "ok")
        .count()
}

/// A hub, a spoke `vic` with an event to push, and the push and the pull of one honest
/// sync of `vic`, as a relay on the path kept them.
fn captured(hub: &Hub) -> (Node, Seen, Seen) {
    let vic = node("vic");
    join(hub, &vic);
    register(&vic.store, "v1");
    let relay = Relay::start(hub.url());
    point_at(&vic, relay.url());
    let kept: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let keep = kept.clone();
    relay.set(move |req, hub| {
        keep.lock().unwrap().push(req.clone());
        hub.send(&req)
    });
    fed::sync(&vic.store).unwrap();
    let kept = kept.lock().unwrap().clone();
    let push = kept.iter().find(|r| r.is_push()).unwrap().clone();
    let pull = kept.iter().find(|r| r.is_pull()).unwrap().clone();
    point_at(&vic, hub.url());
    (vic, push, pull)
}

#[test]
fn a_captured_push_or_pull_sent_again_is_refused_and_serves_nothing() {
    let hub = start_hub();
    let (vic, push, pull) = captured(&hub);
    let pushes = served(&hub, "POST /fed/push");
    let pulls = served(&hub, "GET /fed/pull");
    let contact: Option<i64> = hub
        .node
        .db()
        .query_row(
            "SELECT last_sync_at FROM peers WHERE node_id = ?1",
            [vic.id()],
            |r| r.get(0),
        )
        .unwrap();
    for req in [&push, &pull, &push, &pull] {
        let answer = send(hub.url(), req);
        assert_eq!(
            answer.status,
            401,
            "{}",
            String::from_utf8_lossy(&answer.body)
        );
        assert_eq!(message(&answer), REPLAYED);
        assert_eq!(
            header(&answer, "x-another-signature"),
            None,
            "nothing signed"
        );
        assert!(
            !String::from_utf8_lossy(&answer.body).contains("\"events\""),
            "no feed page"
        );
    }
    assert_eq!(served(&hub, "POST /fed/push"), pushes);
    assert_eq!(served(&hub, "GET /fed/pull"), pulls);
    let after: Option<i64> = hub
        .node
        .db()
        .query_row(
            "SELECT last_sync_at FROM peers WHERE node_id = ?1",
            [vic.id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(after, contact, "no contact recorded for a replay");
    let denied = audit_rows(&hub.node.store, "GET /fed/pull");
    assert!(
        denied
            .iter()
            .any(|a| a.outcome == "denied" && a.detail.as_deref() == Some(REPLAYED)),
        "{denied:?}"
    );
    // The spoke itself syncs on.
    fed::sync(&vic.store).unwrap();
}

/// The finding as it was: a hub of the build before serves the same captured requests
/// again. This is what the change closes.
#[test]
fn a_hub_of_the_build_before_serves_a_captured_request_again() {
    let hub = start_current_main_hub("hub");
    let vic = node("vic");
    join(&hub, &vic);
    register(&vic.store, "v1");
    let relay = Relay::start(hub.url());
    point_at(&vic, relay.url());
    let kept: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let keep = kept.clone();
    relay.set(move |req, hub| {
        keep.lock().unwrap().push(req.clone());
        hub.send(&req)
    });
    current_main_sync(&vic).unwrap();
    let pull = kept
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.is_pull())
        .unwrap()
        .clone();
    let answer = send(hub.url(), &pull);
    assert_eq!(answer.status, 200);
    assert!(header(&answer, "x-another-signature").is_some());
}

#[test]
fn a_captured_request_with_a_new_nonce_or_without_its_v2_signature_is_refused() {
    let hub = start_hub();
    let (_vic, push, pull) = captured(&hub);
    for req in [&push, &pull] {
        let mut renonced = req.clone();
        renonced.set_header("x-another-nonce", &crypto::request_nonce().unwrap());
        let answer = send(hub.url(), &renonced);
        assert_eq!(answer.status, 401);
        assert_eq!(message(&answer), NOT_SIGNED);

        let mut stripped = req.clone();
        stripped.remove_header("x-another-signature-v2");
        let answer = send(hub.url(), &stripped);
        assert_eq!(answer.status, 401);
        assert_eq!(header(&answer, "upgrade"), Some("another-signed-request/2"));
        assert_eq!(message(&answer), N3);
        assert_eq!(header(&answer, "x-another-signature"), None);

        // The older signature in the v2 header does not pass either.
        let mut swapped = req.clone();
        let v1 = req.header("x-another-signature").unwrap().to_string();
        swapped.set_header("x-another-signature-v2", &v1);
        assert_eq!(send(hub.url(), &swapped).status, 401);
    }
}

#[test]
fn a_captured_request_is_refused_by_another_hub_the_spoke_moves_to() {
    let first = start_hub_named("first");
    let (vic, push, _pull) = captured(&first);
    let second = start_hub_named("second");
    vic.store.revoke_peer(&first.id()).unwrap();
    join(&second, &vic);
    let answer = send(second.url(), &push);
    assert_eq!(answer.status, 401);
    assert_eq!(message(&answer), NOT_SIGNED);
    assert_eq!(events_from(&second.node, &vic.id()), 0);
    fed::sync(&vic.store).unwrap();
    assert!(events_from(&second.node, &vic.id()) > 0);
}

#[test]
fn a_captured_request_is_refused_after_the_hub_restarts() {
    let hub = start_hub();
    let (_vic, push, pull) = captured(&hub);
    let Hub { node, served } = hub;
    drop(served);
    let reopened = Store::open(node.dir.path()).unwrap();
    let again = serve(fed::router(reopened));
    for req in [&push, &pull] {
        let answer = send(&again.url, req);
        assert_eq!(answer.status, 401);
        assert_eq!(message(&answer), REPLAYED);
    }
}

#[test]
fn concurrent_copies_of_one_signed_request_are_served_once() {
    let hub = start_hub();
    let (vic, _push, pull) = captured(&hub);
    // A request the hub has not seen yet, signed as the spoke signs.
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let nonce = crypto::request_nonce().unwrap();
    let mut fresh = pull.clone();
    fresh.set_header("x-another-time", &time.to_string());
    fresh.set_header("x-another-nonce", &nonce);
    fresh.set_header(
        "x-another-signature",
        &vic.store.sign_request("GET", &fresh.path, time, b""),
    );
    fresh.set_header(
        "x-another-signature-v2",
        &vic.store
            .sign_request_v2(&hub.id(), "GET", &fresh.path, time, &nonce, b""),
    );
    let statuses: Vec<u16> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (url, req) = (hub.url(), &fresh);
                s.spawn(move || send(url, req).status)
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(
        statuses.iter().filter(|s| **s == 200).count(),
        1,
        "{statuses:?}"
    );
    assert_eq!(
        statuses.iter().filter(|s| **s == 401).count(),
        7,
        "{statuses:?}"
    );
}

#[test]
fn honest_syncs_and_a_retry_after_a_lost_answer_each_use_a_new_nonce() {
    let hub = start_hub();
    let vic = node("vic");
    join(&hub, &vic);
    let relay = Relay::start(hub.url());
    point_at(&vic, relay.url());
    for i in 0..5 {
        register(&vic.store, &format!("v{i}"));
        fed::sync(&vic.store).unwrap();
    }
    // The hub serves a push, and its answer is lost on the way back.
    register(&vic.store, "lost");
    let dropped = Arc::new(Mutex::new(false));
    let once = dropped.clone();
    relay.set(move |req, hub| {
        let answer = hub.send(&req);
        let mut done = once.lock().unwrap();
        if req.is_push() && !*done {
            *done = true;
            return Answer::json(502, &serde_json::json!({ "error": "lost" }));
        }
        answer
    });
    assert!(fed::sync(&vic.store).is_err());
    assert!(*dropped.lock().unwrap());
    let report = fed::sync(&vic.store).expect("the retry is a new request");
    assert_eq!(report.pushed + report.push_duplicates, 1, "{report:?}");
    let nonces: Vec<String> = relay
        .seen()
        .iter()
        .map(|r| r.header("x-another-nonce").unwrap().to_string())
        .collect();
    let mut distinct = nonces.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(
        distinct.len(),
        nonces.len(),
        "every request has its own nonce"
    );
}

// ---------------------------------------------------------------------------
// Mixed versions: the build before replay-proof requests against this one.

#[test]
fn a_new_spoke_pairs_and_syncs_both_ways_with_a_hub_of_the_build_before() {
    let hub = start_current_main_hub("hub");
    let vic = node("vic");
    let other = node("other");
    join(&hub, &vic);
    join(&hub, &other);
    register(&vic.store, "v1");
    register(&other.store, "o1");
    current_main_sync(&other).unwrap();
    let report = fed::sync(&vic.store).expect("a new spoke syncs with the hub of the build before");
    assert!(report.pushed > 0, "{report:?}");
    assert!(report.pulled > 0, "{report:?}");
    assert!(events_from(&hub.node, &vic.id()) > 0, "pushed");
    assert!(events_from(&vic, &other.id()) > 0, "pulled");
    current_main_sync(&other).unwrap();
    assert!(events_from(&other, &vic.id()) > 0, "relayed on");
}

#[test]
fn a_spoke_of_the_build_before_is_refused_by_a_new_hub_then_syncs_once_updated() {
    let hub = start_hub();
    let vic = node("vic");
    join(&hub, &vic);
    register(&vic.store, "v1");
    // The spoke of the build before quotes the hub's own text, which says what to do.
    let err = format!("{:#}", current_main_sync(&vic).unwrap_err());
    assert_eq!(err, format!("push failed (401 Unauthorized): {N3}"));
    assert_eq!(events_from(&hub.node, &vic.id()), 0, "nothing stored");
    let denied = audit_rows(&hub.node.store, "POST /fed/push");
    assert!(denied.iter().all(|a| a.outcome == "denied"), "{denied:?}");
    assert!(
        denied.iter().any(|a| a.detail.as_deref()
            == Some(
                "no x-another-signature-v2 header: the node runs an older build, or \
                 something on the path removed it"
            )),
        "{denied:?}"
    );
    // Updated, with no new pairing.
    fed::sync(&vic.store).unwrap();
    assert!(events_from(&hub.node, &vic.id()) > 0);
}

/// A spoke of this build that meets the refusal meant for an older one knows that a header
/// was removed on the way, and says so instead of telling its owner to update.
#[test]
fn a_relay_that_removes_the_v2_signature_fails_the_sync_and_says_why() {
    let hub = start_hub();
    let vic = node("vic");
    join(&hub, &vic);
    register(&vic.store, "v1");
    let relay = Relay::start(hub.url());
    point_at(&vic, relay.url());
    relay.set(|mut req, hub| {
        req.remove_header("x-another-signature-v2");
        hub.send(&req)
    });
    let err = format!("{:#}", fed::sync(&vic.store).unwrap_err());
    assert_eq!(
        err,
        format!(
            "the hub at {} refused the push as carrying no nonce or no request signature for \
             it, but this node sent both; nothing was applied. Something between this node \
             and the hub removed a header, or the refusal is not the hub's; if a proxy in \
             front of the hub demands TLS, the hub's address may need to be https://",
            relay.url()
        )
    );
    assert_eq!(events_from(&hub.node, &vic.id()), 0);
}
