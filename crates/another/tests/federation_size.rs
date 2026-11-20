//! Sync and size (Q26): a hub and spokes over real loopback HTTP, each in its own temp home.
//! The first two tests state what an honest sync must do and fail on a build that bounds
//! batches and pages by count only. The third records what one oversized honest event does
//! today; what it should do is an open decision.

use std::net::SocketAddr;

use another::fed;
use another::serve::{http_router, HttpOptions};
use another_core::*;
use tokio_util::sync::CancellationToken;

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

fn register(store: &Store, name: &str, description: Option<String>) {
    store
        .register_agent(RegisterAgent {
            name: name.into(),
            program: Program::Claude,
            project: None,
            model: None,
            description,
        })
        .unwrap();
}

struct Hub {
    node: Node,
    url: String,
    ct: CancellationToken,
    _rt: tokio::runtime::Runtime,
}

fn start_hub() -> Hub {
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
    let opts = HttpOptions {
        federation: true,
        bound_port: addr.port(),
        ..Default::default()
    };
    let app = http_router(node.store.clone(), ct.clone(), &opts);
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

fn inbox_subjects(store: &Store, agent: &str) -> Vec<String> {
    store
        .inbox(
            agent,
            InboxQuery {
                unread_only: false,
                urgent_only: false,
                limit: 200,
            },
        )
        .unwrap()
        .messages
        .into_iter()
        .map(|m| m.message.subject)
        .collect()
}

const BODY: usize = 256 * 1024;

/// Nine honest messages with the largest body the mail rule allows, waiting in one push
/// batch: the sync delivers them.
#[test]
fn nine_largest_messages_in_one_push_batch_sync() {
    let hub = start_hub();
    let a = node("spokea");
    join(&hub, &a);
    register(&hub.node.store, "bob", None);
    register(&a.store, "alice", None);
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
/// delivers them.
#[test]
fn forty_five_largest_messages_in_one_pull_page_sync() {
    let hub = start_hub();
    let c = node("spokec");
    join(&hub, &c);
    register(&hub.node.store, "bob", None);
    register(&c.store, "carol", None);
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

/// One honest event that is by itself larger than the hub's request limit. An agent's
/// description has no size rule, so `register_agent` (the CLI and the MCP tool both end
/// there) accepts 3 MiB. Today every sync then fails the same way and the mail written
/// after it never leaves the node. No batching can fix this; it records today's behaviour.
#[test]
fn one_event_over_the_push_limit_blocks_everything_after_it_today() {
    let hub = start_hub();
    let a = node("spokea");
    join(&hub, &a);
    register(&hub.node.store, "bob", None);
    register(&a.store, "alice", None);
    fed::sync(&a.store).unwrap();
    register(&a.store, "wordy", Some("d".repeat(3 * 1024 * 1024)));
    send(&a.store, "alice", "bob@@hub", "after", "small".into());
    let first = fed::sync(&a.store).map(|r| r.pushed);
    let second = fed::sync(&a.store).map(|r| r.pushed);
    let got = inbox_subjects(&hub.node.store, "bob");
    hub.ct.cancel();
    let (first, second) = (
        format!("{:#}", first.unwrap_err()),
        format!("{:#}", second.unwrap_err()),
    );
    eprintln!("first sync: {first}\nsecond sync: {second}\nbob has: {got:?}");
    assert!(first.contains("413"), "{first}");
    assert_eq!(first, second);
    assert!(got.is_empty(), "{got:?}");
}
