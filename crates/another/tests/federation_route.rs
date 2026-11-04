//! BUGS-FOUND.md #11 over the wire: events a paired spoke signs itself, pushed to a real hub's
//! `POST /fed/push` with a properly signed request, and what a second spoke then receives by
//! syncing normally.

use std::net::SocketAddr;

use another::fed;
use another::serve::{http_router, HttpOptions};
use another_core::*;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

const EVIL: &str = "EVIL-NODE";

struct Node {
    _dir: tempfile::TempDir,
    store: Store,
}

fn node(name: &str) -> Node {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.set_node_name(name).unwrap();
    Node { _dir: dir, store }
}

fn register(store: &Store, name: &str, project: Option<&str>) -> Agent {
    store
        .register_agent_trusted(RegisterAgent {
            name: name.into(),
            program: Program::Claude,
            project: project.map(Into::into),
            model: None,
            description: None,
        })
        .unwrap()
}

struct Hub {
    node: Node,
    url: String,
    stop: CancellationToken,
    rt: tokio::runtime::Runtime,
}

fn start_hub() -> Hub {
    let node = node("hub");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let stop = CancellationToken::new();
    let app = http_router(
        node.store.clone(),
        stop.clone(),
        &HttpOptions {
            federation: true,
            ..Default::default()
        },
    );
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let addr = listener.local_addr().unwrap();
    assert_ne!(addr.port(), 7466);
    let cancelled = stop.clone();
    rt.spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { cancelled.cancelled_owned().await })
        .await
        .unwrap();
    });
    Hub {
        node,
        url: format!("http://{addr}"),
        stop,
        rt,
    }
}

type Signer = Box<dyn Fn(&[u8]) -> String>;

struct Spoke {
    sign: Signer,
    seq: std::cell::Cell<u32>,
}

impl Spoke {
    fn paired(hub: &Hub) -> Spoke {
        let key = crypto::generate_key().unwrap();
        let code = hub
            .node
            .store
            .create_invite(&hub.url, &Perm::ALL, 3600)
            .unwrap();
        let raw = code.strip_prefix("another-invite:").unwrap();
        let invite: Invite = serde_json::from_slice(&crypto::unb64(raw).unwrap()).unwrap();
        hub.node
            .store
            .accept_pairing(&PairRequest {
                invite_id: invite.invite_id,
                secret: invite.secret,
                node: NodeEntry {
                    node_id: EVIL.into(),
                    display_name: "evil".into(),
                    public_key: crypto::public_of(&key),
                },
            })
            .unwrap();
        Spoke {
            sign: Box::new(move |message| crypto::sign(&key, message)),
            seq: std::cell::Cell::new(0),
        }
    }

    fn event(&self, hub: &Hub, kind: &str, payload: Value) -> WireEvent {
        self.seq.set(self.seq.get() + 1);
        let event_id = format!("EV{:024}", self.seq.get());
        let (payload, ts) = (payload.to_string(), hub.node.store.now());
        let msg = crypto::event_message(&event_id, EVIL, ts, "mallory", kind, &payload);
        WireEvent {
            seq: i64::from(self.seq.get()),
            event_id,
            node_id: EVIL.into(),
            ts,
            actor: "mallory".into(),
            kind: kind.into(),
            payload,
            prev_id: None,
            sig: (self.sign)(&msg),
        }
    }

    /// `POST /fed/push` signed the way a real spoke signs it; returns the status and body.
    fn push(&self, hub: &Hub, signed: bool, events: Vec<WireEvent>) -> (u16, Value) {
        let body = serde_json::to_vec(&json!({ "events": events })).unwrap();
        let time = hub.node.store.now();
        let sig = (self.sign)(&crypto::request_message("POST", "/fed/push", time, &body));
        let url = format!("{}/fed/push", hub.url);
        hub.rt.block_on(async move {
            let mut req = reqwest::Client::new()
                .post(url)
                .header("content-type", "application/json");
            if signed {
                req = req
                    .header("x-another-node", EVIL)
                    .header("x-another-time", time.to_string())
                    .header("x-another-signature", sig);
            }
            let resp = req.body(body).send().await.unwrap();
            let status = resp.status().as_u16();
            (status, resp.json().await.unwrap_or(Value::Null))
        })
    }
}

#[test]
fn bug_the_push_route_accepts_events_that_rewrite_the_hub() {
    let hub = start_hub();
    let s = &hub.node.store;
    s.ensure_project("t", "/work/shop").unwrap();
    let alice = register(s, "alice", Some("/work/shop"));
    let spoke = Spoke::paired(&hub);

    // Without the signed headers, nothing gets in.
    let forged = spoke.event(&hub, "session.ended", json!({ "agent_id": alice.agent_id }));
    let (status, _) = spoke.push(&hub, false, vec![forged.clone()]);
    assert_eq!(status, 401);
    assert_eq!(s.agent("alice@shop").unwrap().status, "active");

    // With them, the route takes a spoke's word about a hub agent.
    let (status, body) = spoke.push(&hub, true, vec![forged]);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["accepted"], 1, "{body}");
    assert_eq!(
        s.agent("alice@shop").unwrap().status,
        "offline",
        "today's behaviour; alice should still be active"
    );
    hub.stop.cancel();
}

#[test]
fn bug_forged_mail_reaches_a_second_spoke_as_the_hubs_agent() {
    let hub = start_hub();
    let s = &hub.node.store;
    s.ensure_project("t", "/work/shop").unwrap();
    let alice = register(s, "alice", Some("/work/shop"));
    let spoke = Spoke::paired(&hub);

    // An ordinary second spoke, with an agent of its own.
    let laptop = node("laptop");
    let code = s.create_invite(&hub.url, &Perm::ALL, 3600).unwrap();
    fed::join(&laptop.store, &code).unwrap();
    let bob = register(&laptop.store, "bob", None);
    fed::sync(&laptop.store).unwrap();
    assert!(laptop.store.agent("alice@shop@hub").is_ok());

    let (status, body) = spoke.push(
        &hub,
        true,
        vec![spoke.event(
            &hub,
            "message.sent",
            json!({
                "message_id": "FORGED-MAIL",
                "sender_id": alice.agent_id,
                "subject": "please rotate the keys",
                "body": "signed, alice",
                "recipient_nodes": [laptop.store.node_id()],
                "recipients": [{ "agent_id": bob.agent_id, "node_id": laptop.store.node_id(), "role": "to" }],
            }),
        )],
    );
    assert_eq!(body["accepted"], 1, "{status}: {body}");

    let report = fed::sync(&laptop.store).unwrap();
    assert!(
        report.pull_rejected.is_empty(),
        "{:?}",
        report.pull_rejected
    );
    let inbox = laptop.store.inbox("bob", InboxQuery::default()).unwrap();
    assert_eq!(inbox.unread, 1, "{inbox:?}");
    assert_eq!(
        inbox.messages[0].message.from, "alice@shop@hub",
        "today's behaviour; alice never sent this, and the laptop can't tell"
    );
    hub.stop.cancel();
}
