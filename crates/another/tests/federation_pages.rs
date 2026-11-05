//! A pull that spans pages over real HTTP. Requests are signed with the system time, so the hub's
//! clock stays at the present while the test moves the owner's and borrower's for the leases.

use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use another::fed;
use another::serve::{http_router, HttpOptions};
use another_core::*;
use tokio_util::sync::CancellationToken;

const MINUTE: i64 = 60_000;
/// More than one pull page, so a page boundary falls between the events either side of this padding.
const PADDING: usize = 600;

fn node(name: &str, agents: &[&str], now: i64) -> (Store, Arc<AtomicI64>) {
    let (clock, time) = Clock::manual(now);
    let store = Store::open_in_memory(clock).unwrap();
    store.set_node_name(name).unwrap();
    for agent in agents {
        store
            .register_agent_trusted(RegisterAgent {
                name: (*agent).into(),
                program: Program::Codex,
                project: None,
                model: None,
                description: None,
            })
            .unwrap();
    }
    (store, time)
}

fn sync(store: &Store) -> fed::SyncReport {
    let report = fed::sync(store).unwrap();
    assert!(report.push_rejected.is_empty(), "{report:?}");
    assert!(report.pull_rejected.is_empty(), "{report:?}");
    report
}

#[test]
fn a_renewal_on_a_later_pull_page_decides_who_a_lapsed_resource_passes_to() {
    let start = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let (hub, _) = node("hub", &[], start);
    let (owner, owner_time) = node("owner", &["worker"], start);
    let (borrower, borrower_time) = node("borrower", &["old", "new"], start);
    let at = |minutes: i64| {
        for time in [&owner_time, &borrower_time] {
            time.store(start + minutes * MINUTE, Ordering::SeqCst);
        }
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let ct = CancellationToken::new();
    let opts = HttpOptions {
        federation: true,
        ..Default::default()
    };
    let app = http_router(hub.clone(), ct.clone(), &opts);
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
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
    for spoke in [&owner, &borrower] {
        let code = hub.create_invite(&url, &Perm::ALL, 3600).unwrap();
        fed::join(spoke, &code).unwrap();
    }

    let reachable = TcpListener::bind("127.0.0.1:0").unwrap();
    let resource = owner
        .register_resource(RegisterResource {
            agent: "worker".into(),
            kind: ResourceKind::ModelServer,
            name: "shared".into(),
            endpoint: Some(format!("http://{}", reachable.local_addr().unwrap())),
            port: None,
            pid: None,
            project: None,
            purpose: None,
            mode: LeaseMode::Shared,
            ttl_seconds: Some(3600),
            metadata: None,
        })
        .unwrap()
        .resource;
    let id = ResourceRef::Id(resource.resource_id);
    sync(&owner);
    sync(&borrower);

    borrower.borrow_resource("old", &id, Some(1800)).unwrap();
    sync(&borrower);
    at(10);
    borrower.borrow_resource("new", &id, Some(7200)).unwrap();
    sync(&borrower);
    for n in 0..PADDING {
        hub.register_agent_trusted(RegisterAgent {
            name: format!("pad{n}"),
            program: Program::Codex,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    }
    at(20);
    borrower.borrow_resource("old", &id, Some(7200)).unwrap();
    sync(&borrower);

    at(61);
    owner.sweep().unwrap();
    assert_eq!(owner.resource(&id).unwrap().status, "lapsed");

    at(100);
    let pulled = sync(&owner).pulled;
    assert!(pulled > PADDING, "pulled {pulled}");
    assert_eq!(
        owner.resource(&id).unwrap().owner.as_deref(),
        Some("old@@borrower")
    );
    ct.cancel();
}
