use another_core::*;

fn setup() -> (Store, ResourceRef, String) {
    let store = Store::open_in_memory(Clock::System).unwrap();
    for name in ["alice", "bob"] {
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
    let outcome = store
        .register_resource(RegisterResource {
            agent: "alice".into(),
            kind: ResourceKind::DevServer,
            name: "guard-test".into(),
            endpoint: None,
            port: None,
            pid: Some(4242),
            project: None,
            purpose: None,
            mode: LeaseMode::Shared,
            ttl_seconds: None,
            metadata: None,
        })
        .unwrap();
    let id = outcome.resource.resource_id;
    let target = ResourceRef::Id(id.clone());
    store.release_resource("alice", &target, false).unwrap();
    (store, target, id)
}

#[test]
fn record_reap_never_releases_a_new_live_lease() {
    let (store, target, id) = setup();
    store.borrow_resource("bob", &target, None).unwrap();
    assert!(store
        .record_reap("human", Some(&id), Some(4242), "test")
        .is_err());
    assert_eq!(store.resource(&target).unwrap().status, "live");
}

#[test]
fn guarded_reap_refuses_live_lease_without_calling_stop() {
    let (store, target, id) = setup();
    store.borrow_resource("bob", &target, None).unwrap();
    let mut called = false;
    let result = store.guarded_reap(Some(&id), 4242, 0, "test", || {
        called = true;
        Ok(true)
    });
    assert!(result.is_err());
    assert!(!called);
    assert_eq!(store.resource(&target).unwrap().status, "live");
}

#[test]
fn guarded_reap_serializes_borrow_until_after_release() {
    use std::sync::mpsc;
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    for name in ["alice", "bob"] {
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
    let resource = store
        .register_resource(RegisterResource {
            agent: "alice".into(),
            kind: ResourceKind::DevServer,
            name: "guard-race".into(),
            endpoint: None,
            port: None,
            pid: Some(4242),
            project: None,
            purpose: None,
            mode: LeaseMode::Shared,
            ttl_seconds: None,
            metadata: None,
        })
        .unwrap()
        .resource;
    let id = resource.resource_id;
    let target = ResourceRef::Id(id.clone());
    store.release_resource("alice", &target, false).unwrap();
    let second = Store::open(dir.path()).unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (continue_tx, continue_rx) = mpsc::channel();
    let stop = std::thread::spawn(move || {
        store.guarded_reap(Some(&id), 4242, 0, "test", || {
            entered_tx.send(()).unwrap();
            continue_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(true)
        })
    });
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (borrow_tx, borrow_rx) = mpsc::channel();
    let (started_tx, started_rx) = mpsc::channel();
    let borrow = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        borrow_tx
            .send(second.borrow_resource("bob", &target, None))
            .unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(
        borrow_rx.recv_timeout(Duration::from_millis(150)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    continue_tx.send(()).unwrap();
    assert!(stop.join().unwrap().unwrap());
    assert!(borrow_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .is_err());
    borrow.join().unwrap();
}

#[test]
fn callback_error_reports_unknown_outcome_and_keeps_resource_lapsed() {
    let (store, target, id) = setup();
    let error = store
        .guarded_reap(Some(&id), 4242, 0, "test", || {
            Err(Error::Invalid("injected stop failure".into()))
        })
        .unwrap_err();
    assert!(error.to_string().contains("outcome may be unknown"));
    assert_eq!(store.resource(&target).unwrap().status, "lapsed");
}

#[test]
fn guarded_reap_refuses_any_other_active_resource_with_same_pid() {
    let (store, _, id) = setup();
    store
        .register_resource(RegisterResource {
            agent: "bob".into(),
            kind: ResourceKind::DevServer,
            name: "other".into(),
            endpoint: None,
            port: None,
            pid: Some(4242),
            project: None,
            purpose: None,
            mode: LeaseMode::Shared,
            ttl_seconds: None,
            metadata: None,
        })
        .unwrap();
    let mut called = false;
    assert!(store
        .guarded_reap(Some(&id), 4242, 0, "test", || {
            called = true;
            Ok(true)
        })
        .is_err());
    assert!(!called);
    assert!(store
        .guarded_reap(None, 4242, 0, "test", || Ok(true))
        .is_err());
}

#[test]
fn guarded_reap_serializes_new_pid_alias_registration() {
    use std::sync::mpsc;
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store
        .register_agent(RegisterAgent {
            name: "alice".into(),
            program: Program::Claude,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    let second = Store::open(dir.path()).unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (continue_tx, continue_rx) = mpsc::channel();
    let stop = std::thread::spawn(move || {
        store.guarded_reap(None, 4242, 0, "test", || {
            entered_tx.send(()).unwrap();
            continue_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(true)
        })
    });
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (started_tx, started_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    let register = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        let outcome = second.register_resource(RegisterResource {
            agent: "alice".into(),
            kind: ResourceKind::DevServer,
            name: "new-alias".into(),
            endpoint: None,
            port: None,
            pid: Some(4242),
            project: None,
            purpose: None,
            mode: LeaseMode::Shared,
            ttl_seconds: None,
            metadata: None,
        });
        result_tx.send(outcome).unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(
        result_rx.recv_timeout(Duration::from_millis(150)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    continue_tx.send(()).unwrap();
    assert!(stop.join().unwrap().unwrap());
    let later = result_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    assert_eq!(later.resource.status, "live");
    register.join().unwrap();
}
