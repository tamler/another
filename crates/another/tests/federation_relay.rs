//! What something that answers in front of the real hub can do to a spoke (Q29). Each test
//! puts a relay between a spoke and its hub: at an address edited into an invite, or on the
//! path of a plain `http://` link. The relay forwards to the hub and edits what the test
//! says. It can drop traffic, which fails the sync loudly; it cannot make the spoke apply
//! anything the hub did not sign for the request the spoke made.
//!
//! The spoke of each test joined the hub directly; its stored address is then pointed at
//! the relay, except in the first test, where the invite itself is edited.

#[path = "common/relay.rs"]
mod relay;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use another::fed;
use another_core::*;
use relay::*;
use serde_json::{json, Value};

const J2: &str = "signature does not match its contents";
const U1_PUSH: &str = "answered the push without a signature";
const U2_PUSH: &str = "answered the push with a signature that is not the hub's";
const U2_PULL: &str = "answered the pull with a signature that is not the hub's";
const U3: &str = "as carrying no nonce, but this node sent one";

/// The sync must fail, and say `fragment`.
fn refused(spoke: &Node, fragment: &str) -> String {
    let err = match fed::sync(&spoke.store) {
        Ok(report) => panic!("the sync went through: {report:?}"),
        Err(e) => format!("{e:#}"),
    };
    assert!(err.contains(fragment), "expected {fragment:?} in: {err}");
    err
}

/// A hub with two spokes paired at its real address: `vic`, whose traffic the relay will
/// sit on, and `late`, another node whose events reach `vic` through the hub.
fn hub_and_two() -> (Hub, Node, Node) {
    let hub = start_fed_hub();
    let vic = node("vic");
    let late = node("late");
    join(&hub, &vic);
    join(&hub, &late);
    (hub, vic, late)
}

#[test]
fn an_invite_with_an_edited_address_is_refused_before_sending() {
    let hub = start_fed_hub();
    let vic = node("vic");
    let relay = Relay::start(hub.url());
    let code = hub
        .node
        .store
        .create_invite(hub.url(), &Perm::ALL, 3600)
        .unwrap();
    let mut edited = decode_invite(&code);
    edited["hub_url"] = json!(relay.url());

    let err = match fed::join(&vic.store, &encode_invite(&edited)) {
        Ok(peer) => panic!("joined through the edited address: {peer:?}"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains(J2), "{err}");
    assert!(
        relay.seen().is_empty(),
        "nothing was sent to the edited address"
    );
    assert!(vic.store.peers().unwrap().is_empty(), "no hub recorded");
    let used: i64 = hub
        .node
        .db()
        .query_row(
            "SELECT count(*) FROM invites WHERE used_at IS NOT NULL OR used_by IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(used, 0, "the invite is still unused on the hub");
    // The genuine code still joins.
    fed::join(&vic.store, &code).unwrap();
}

#[test]
fn a_relay_that_answers_a_push_itself_cannot_report_it_pushed() {
    let hub = start_fed_hub();
    let vic = node("vic");
    join(&hub, &vic);
    register(&vic.store, "v1");
    let relay = Relay::start(hub.url());
    point_at(&vic, relay.url());
    relay.set(|req, hub| {
        if !req.is_push() {
            return hub.send(&req);
        }
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        let n = body["events"].as_array().unwrap().len();
        Answer::json(
            200,
            &json!({ "accepted": n, "duplicates": 0, "rejected": [] }),
        )
    });
    let before = sync_state(&vic, &hub.id());

    refused(&vic, U1_PUSH);
    assert_eq!(
        sync_state(&vic, &hub.id()).push_cursor,
        before.push_cursor,
        "the push cursor did not move"
    );
    assert_eq!(events_of_kind(&vic, "peer.pushed"), 0, "no pushed anchor");
    assert_eq!(
        events_from(&hub.node, &vic.id()),
        0,
        "the hub holds nothing"
    );

    relay.honest();
    fed::sync(&vic.store).unwrap();
    assert!(
        events_from(&hub.node, &vic.id()) > 0,
        "the events reach the hub"
    );
}

#[test]
fn a_relay_cannot_withhold_events_from_a_pull() {
    let (hub, vic, late) = hub_and_two();
    register(&late.store, "l1");
    fed::sync(&late.store).unwrap();
    let relay = Relay::start(hub.url());
    point_at(&vic, relay.url());
    relay.set(|req, hub| {
        if !req.is_pull() {
            return hub.send(&req);
        }
        hub.send(&req).edit(|v| v["events"] = json!([]))
    });
    let before = sync_state(&vic, &hub.id());

    refused(&vic, U2_PULL);
    assert_eq!(
        sync_state(&vic, &hub.id()).pull_cursor,
        before.pull_cursor,
        "the pull cursor did not move"
    );
    assert_eq!(events_from(&vic, &late.id()), 0);

    relay.honest();
    fed::sync(&vic.store).unwrap();
    assert!(events_from(&vic, &late.id()) > 0, "the events arrive");
}

#[test]
fn a_relay_cannot_reorder_events_in_a_pull() {
    let (hub, vic, late) = hub_and_two();
    register(&late.store, "l1");
    register(&late.store, "l2");
    fed::sync(&late.store).unwrap();
    let relay = Relay::start(hub.url());
    point_at(&vic, relay.url());
    let reordered = Arc::new(AtomicUsize::new(0));
    let count = reordered.clone();
    relay.set(move |req, hub| {
        if !req.is_pull() {
            return hub.send(&req);
        }
        hub.send(&req).edit(|v| {
            let events = v["events"].as_array_mut().unwrap();
            count.store(events.len(), Ordering::SeqCst);
            events.reverse();
        })
    });
    // The push of this sync is honest, so only what a pull writes is compared.
    let before = sync_state(&vic, &hub.id());

    refused(&vic, U2_PULL);
    assert!(
        reordered.load(Ordering::SeqCst) >= 2,
        "the page held events to reorder"
    );
    let after = sync_state(&vic, &hub.id());
    assert_eq!(after.pull_cursor, before.pull_cursor);
    assert_eq!(after.anchors.received, before.anchors.received);
    assert_eq!(after.nodes, before.nodes);
    assert_eq!(events_from(&vic, &late.id()), 0, "nothing was applied");
}

#[test]
fn a_relay_cannot_add_a_node_with_its_own_key() {
    let hub = start_fed_hub();
    let vic = node("vic");
    let fourth = node("fourth");
    join(&hub, &vic);
    join(&hub, &fourth);
    register(&fourth.store, "f1");
    fed::sync(&fourth.store).unwrap();
    let fourth_id = fourth.id();
    let real_key = fourth.store.node_entry().unwrap().public_key;
    let relay_key = crypto::public_of(&crypto::generate_key().unwrap());
    assert_eq!(pinned_key(&vic, &fourth_id), None, "not seen yet");

    let relay = Relay::start(hub.url());
    point_at(&vic, relay.url());
    let (id, key) = (fourth_id.clone(), relay_key.clone());
    relay.set(move |req, hub| {
        if !req.is_pull() {
            return hub.send(&req);
        }
        hub.send(&req).edit(|v| {
            for n in v["nodes"].as_array_mut().unwrap() {
                if n["node_id"] == json!(id) {
                    n["public_key"] = json!(key);
                }
            }
        })
    });

    refused(&vic, U2_PULL);
    assert_eq!(
        pinned_key(&vic, &fourth_id),
        None,
        "no row for the node the relay listed"
    );

    relay.honest();
    fed::sync(&vic.store).unwrap();
    assert_eq!(pinned_key(&vic, &fourth_id), Some(Some(real_key)));
    assert!(
        events_from(&vic, &fourth_id) > 0,
        "that node's events apply"
    );
}

#[test]
fn a_replayed_signed_answer_is_refused() {
    let (hub, vic, late) = hub_and_two();
    register(&vic.store, "v1");
    register(&late.store, "l1");
    fed::sync(&late.store).unwrap();
    let relay = Relay::start(hub.url());
    point_at(&vic, relay.url());

    // One honest sync, of which the relay keeps the hub's answers, headers and all.
    let kept: Arc<Mutex<HashMap<&'static str, Answer>>> = Arc::default();
    let keep = kept.clone();
    relay.set(move |req, hub| {
        let answer = hub.send(&req);
        let what = if req.is_push() { "push" } else { "pull" };
        keep.lock()
            .unwrap()
            .entry(what)
            .or_insert_with(|| answer.clone());
        answer
    });
    fed::sync(&vic.store).unwrap();
    let old_push = kept.lock().unwrap()["push"].clone();
    let old_pull = kept.lock().unwrap()["pull"].clone();

    // The next pull gets the old pull answer.
    register(&late.store, "l2");
    fed::sync(&late.store).unwrap();
    relay.set(move |req, hub| {
        if req.is_pull() {
            old_pull.clone()
        } else {
            hub.send(&req)
        }
    });
    let before = sync_state(&vic, &hub.id());
    let held = events_from(&vic, &late.id());
    refused(&vic, U2_PULL);
    assert_eq!(sync_state(&vic, &hub.id()).pull_cursor, before.pull_cursor);
    assert_eq!(events_from(&vic, &late.id()), held);

    // The next push gets the old push answer.
    register(&vic.store, "v2");
    relay.set(move |req, hub| {
        if req.is_push() {
            old_push.clone()
        } else {
            hub.send(&req)
        }
    });
    let before = sync_state(&vic, &hub.id());
    refused(&vic, U2_PUSH);
    let after = sync_state(&vic, &hub.id());
    assert_eq!(after.push_cursor, before.push_cursor);
    assert_eq!(after.pull_cursor, before.pull_cursor);
}

#[test]
fn an_earlier_request_sent_in_place_of_this_one_is_refused() {
    let (hub, vic, late) = hub_and_two();
    register(&vic.store, "v1");
    register(&late.store, "l1");
    fed::sync(&late.store).unwrap();
    let relay = Relay::start(hub.url());
    point_at(&vic, relay.url());

    // One honest sync, of which the relay keeps the spoke's signed requests.
    let kept: Arc<Mutex<HashMap<&'static str, Seen>>> = Arc::default();
    let keep = kept.clone();
    relay.set(move |req, hub| {
        let what = if req.is_push() { "push" } else { "pull" };
        keep.lock()
            .unwrap()
            .entry(what)
            .or_insert_with(|| req.clone());
        hub.send(&req)
    });
    fed::sync(&vic.store).unwrap();
    let old_push = kept.lock().unwrap()["push"].clone();
    let old_pull = kept.lock().unwrap()["pull"].clone();

    // The earlier request, still inside its five minutes, goes to the hub with the nonce
    // of the request the spoke just made; the hub's signed answer to it comes back.
    let in_place = |old: Seen, now: &Seen| {
        let mut sent = old;
        if let Some(nonce) = now.header("x-another-nonce") {
            sent.set_header("x-another-nonce", nonce);
        }
        sent
    };

    register(&late.store, "l2");
    fed::sync(&late.store).unwrap();
    relay.set(move |req, hub| {
        if req.is_pull() {
            hub.send(&in_place(old_pull.clone(), &req))
        } else {
            hub.send(&req)
        }
    });
    let before = sync_state(&vic, &hub.id());
    refused(&vic, U2_PULL);
    assert_eq!(sync_state(&vic, &hub.id()).pull_cursor, before.pull_cursor);

    register(&vic.store, "v2");
    relay.set(move |req, hub| {
        if req.is_push() {
            hub.send(&in_place(old_push.clone(), &req))
        } else {
            hub.send(&req)
        }
    });
    let before = sync_state(&vic, &hub.id());
    refused(&vic, U2_PUSH);
    let after = sync_state(&vic, &hub.id());
    assert_eq!(after.push_cursor, before.push_cursor);
    assert_eq!(after.pull_cursor, before.pull_cursor);
}

#[test]
fn answers_swapped_between_two_spokes_are_refused() {
    let hub = start_fed_hub();
    let alpha = node("alpha");
    let beta = node("beta");
    join(&hub, &alpha);
    join(&hub, &beta);
    register(&alpha.store, "a1");
    register(&beta.store, "b1");
    let relay = Relay::start(hub.url());
    point_at(&alpha, relay.url());
    point_at(&beta, relay.url());

    // Each pull waits for the other spoke's, and gets the hub's answer to that one.
    let answers: Arc<(Mutex<HashMap<String, Answer>>, Condvar)> = Arc::default();
    relay.set(move |req, hub| {
        if !req.is_pull() {
            return hub.send(&req);
        }
        let me = req.header("x-another-node").unwrap().to_string();
        let mine = hub.send(&req);
        let (held, arrived) = &*answers;
        let mut held = held.lock().unwrap();
        held.insert(me.clone(), mine.clone());
        arrived.notify_all();
        let (held, _) = arrived
            .wait_timeout_while(held, Duration::from_secs(20), |h| {
                !h.keys().any(|k| *k != me)
            })
            .unwrap();
        let other = held.iter().find(|(k, _)| **k != me).map(|(_, a)| a.clone());
        other.unwrap_or(mine)
    });

    let results: Vec<Result<String, String>> = std::thread::scope(|s| {
        let handles: Vec<_> = [&alpha, &beta]
            .into_iter()
            .map(|spoke| {
                s.spawn(move || match fed::sync(&spoke.store) {
                    Ok(report) => Ok(format!("{report:?}")),
                    Err(e) => Err(format!("{e:#}")),
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for result in results {
        let err = result.expect_err("a spoke applied the other spoke's answer");
        assert!(err.contains(U2_PULL), "{err}");
    }
}

#[test]
fn a_relay_that_removes_the_nonce_fails_the_sync() {
    let hub = start_fed_hub();
    let vic = node("vic");
    join(&hub, &vic);
    register(&vic.store, "v1");
    let relay = Relay::start(hub.url());
    point_at(&vic, relay.url());
    relay.set(|mut req, hub| {
        req.remove_header("x-another-nonce");
        hub.send(&req)
    });

    refused(&vic, U3);
    assert_eq!(
        events_from(&hub.node, &vic.id()),
        0,
        "nothing stored on the hub"
    );
    let rows = audit_rows(&hub.node.store, "POST /fed/push");
    let denied: Vec<_> = rows.iter().filter(|a| a.outcome == "denied").collect();
    assert_eq!(denied.len(), 1, "{rows:?}");
    assert_eq!(
        denied[0].detail.as_deref(),
        Some(
            "no x-another-nonce header: the node runs an older build, or something on the \
             path removed it"
        )
    );
    assert_eq!(
        denied[0].agent.as_deref(),
        Some(format!("node:{}", vic.id()).as_str())
    );
}

#[test]
fn a_relay_that_changes_the_nonce_fails_the_sync() {
    let hub = start_fed_hub();
    let vic = node("vic");
    join(&hub, &vic);
    register(&vic.store, "v1");
    let relay = Relay::start(hub.url());
    point_at(&vic, relay.url());
    relay.set(|mut req, hub| {
        req.set_header("x-another-nonce", "000102030405060708090a0b0c0d0e0f");
        hub.send(&req)
    });
    let before = sync_state(&vic, &hub.id());

    refused(&vic, U2_PUSH);
    assert_eq!(sync_state(&vic, &hub.id()), before);
}

#[test]
fn a_relay_cannot_hide_a_reset() {
    let (hub, vic, late) = hub_and_two();
    // The operator's backup of the hub, taken before the work below.
    let restored_home = tempfile::tempdir().unwrap();
    let backup = restored_home.path().join(DB_FILE);
    hub.node
        .db()
        .execute("VACUUM INTO ?1", [backup.to_string_lossy()])
        .unwrap();
    register(&late.store, "l1");
    fed::sync(&late.store).unwrap();
    register(&vic.store, "v1");
    fed::sync(&vic.store).unwrap();
    assert!(sync_state(&vic, &hub.id()).pull_cursor > 0);

    // The hub restored from that copy, behind the relay.
    let restored = Store::open(restored_home.path()).unwrap();
    assert_eq!(restored.node_id(), hub.id());
    let restored_hub = serve_store(restored, HubMode::FedRoutes);
    let relay = Relay::start(&restored_hub.url);
    point_at(&vic, relay.url());
    let saw_reset = Arc::new(AtomicBool::new(false));
    let saw = saw_reset.clone();
    relay.set(move |req, hub| {
        if !req.is_pull() {
            return hub.send(&req);
        }
        hub.send(&req).edit(|v| {
            if v["reset"] == json!(true) {
                saw.store(true, Ordering::SeqCst);
            }
            v["reset"] = json!(false);
        })
    });
    let before = sync_state(&vic, &hub.id());

    refused(&vic, U2_PULL);
    assert!(
        saw_reset.load(Ordering::SeqCst),
        "the restored hub reported the reset"
    );
    let after = sync_state(&vic, &hub.id());
    assert_eq!(after.pull_cursor, before.pull_cursor);
    assert_eq!(after.anchors, before.anchors);
}

#[test]
fn forged_error_answers_change_no_state() {
    let (hub, vic, late) = hub_and_two();
    register(&vic.store, "v1");
    register(&late.store, "l1");
    fed::sync(&late.store).unwrap();
    fed::sync(&vic.store).unwrap();
    let relay = Relay::start(hub.url());
    point_at(&vic, relay.url());
    let forged = |status: u16| Answer::json(status, &json!({ "error": { "message": "forged" } }));

    // Push: this node has an event to send, and every request is answered with an error.
    register(&vic.store, "v2");
    for status in [401, 500, 413] {
        relay.set(move |_, _| forged(status));
        let before = sync_state(&vic, &hub.id());
        assert!(fed::sync(&vic.store).is_err(), "push {status}");
        assert_eq!(sync_state(&vic, &hub.id()), before, "push {status}");
    }

    // Pull. One honest sync through the relay first, which pulls an event: that leaves
    // this node's log ending in records that never leave it, so the next sync sends no
    // push and moves the push cursor over them. Its pull is then answered with an error.
    register(&late.store, "l2");
    fed::sync(&late.store).unwrap();
    relay.honest();
    fed::sync(&vic.store).unwrap();
    relay.forget();
    let over_the_read_limit = PULL_BODY_LIMIT_BYTES + 1;
    for case in ["401", "500", "413", "oversize"] {
        relay.set(move |req, hub| {
            if !req.is_pull() {
                return hub.send(&req);
            }
            match case {
                "oversize" => Answer {
                    status: 200,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body: vec![b' '; over_the_read_limit],
                },
                status => forged(status.parse().unwrap()),
            }
        });
        let before = sync_state(&vic, &hub.id());
        assert!(fed::sync(&vic.store).is_err(), "pull {case}");
        let after = sync_state(&vic, &hub.id());
        assert_eq!(after.last_sync_at, before.last_sync_at, "pull {case}");
        assert_eq!(after.pull_cursor, before.pull_cursor, "pull {case}");
        assert_eq!(after.anchors, before.anchors, "pull {case}");
        assert_eq!(after.nodes, before.nodes, "pull {case}");
    }
    let seen = relay.seen();
    assert!(
        seen.iter().all(|r| r.is_pull()),
        "no push was sent: the push cursor moved only over records that stay on this node"
    );
    // The last case asks again with half the count, down to one event, then gives up.
    let oversize_tries = seen.len() - 3;
    assert!(
        (2..=10).contains(&oversize_tries),
        "{oversize_tries} requests"
    );
}
