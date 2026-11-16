//! What resource registration and the guarded reap refuse before they act: metadata that
//! is not a JSON object, and a grace period that is negative or too large to count in
//! milliseconds. A refused reap never calls the stop callback.

use another_core::*;
use serde_json::{json, Value};

fn store() -> Store {
    let store = Store::open_in_memory(Clock::System).unwrap();
    store
        .register_agent(RegisterAgent {
            name: "alice".into(),
            program: Program::Claude,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    store
}

fn register(store: &Store, metadata: Option<Value>) -> Result<LeaseOutcome> {
    store.register_resource(RegisterResource {
        agent: "alice".into(),
        kind: ResourceKind::DevServer,
        name: "refusal-test".into(),
        endpoint: None,
        port: None,
        pid: Some(4242),
        project: None,
        purpose: None,
        mode: LeaseMode::Shared,
        ttl_seconds: None,
        metadata,
    })
}

fn everything(store: &Store) -> Vec<Resource> {
    store
        .find_resources(ResourceFilter {
            include_released: true,
            ..Default::default()
        })
        .unwrap()
}

#[test]
fn metadata_that_is_not_an_object_is_refused_and_nothing_is_registered() {
    let s = store();
    let before = s.events(10_000).unwrap().len();
    for bad in [
        json!(null),
        json!([1, 2]),
        json!("gpu"),
        json!(42),
        json!(true),
    ] {
        let err = register(&s, Some(bad.clone())).unwrap_err();
        assert_eq!(err.code(), "invalid", "{bad}: {err}");
        assert!(
            err.to_string().contains("metadata must be a JSON object"),
            "{bad}: {err}"
        );
    }
    assert!(everything(&s).is_empty());
    assert_eq!(s.events(10_000).unwrap().len(), before);

    // Omitted metadata is stored as an empty object; an object is kept as given.
    let none = register(&s, None).unwrap();
    assert_eq!(none.resource.metadata, json!({}));
    let s = store();
    let given = register(&s, Some(json!({ "backend": "metal" }))).unwrap();
    assert_eq!(given.resource.metadata, json!({ "backend": "metal" }));
}

/// A registered resource whose only lease is released: lapsed, so eligible for a reap.
fn lapsed() -> (Store, String) {
    let s = store();
    let id = register(&s, None).unwrap().resource.resource_id;
    s.release_resource("alice", &ResourceRef::Id(id.clone()), false)
        .unwrap();
    assert_eq!(
        s.resource(&ResourceRef::Id(id.clone())).unwrap().status,
        "lapsed"
    );
    (s, id)
}

#[test]
fn a_negative_or_oversized_grace_is_refused_without_calling_stop() {
    for (grace, message) in [
        (-1, "grace must not be negative"),
        (i64::MIN, "grace must not be negative"),
        (i64::MAX, "grace is too large"),
    ] {
        let (s, id) = lapsed();
        let mut called = false;
        let err = s
            .guarded_reap(Some(&id), 4242, grace, "test", || {
                called = true;
                Ok(true)
            })
            .unwrap_err();
        assert_eq!(err.code(), "invalid", "{grace}: {err}");
        assert!(err.to_string().contains(message), "{grace}: {err}");
        assert!(!called, "{grace}: stop must not run");
        assert_eq!(
            s.resource(&ResourceRef::Id(id)).unwrap().status,
            "lapsed",
            "{grace}"
        );
    }
}

#[test]
fn a_zero_grace_on_the_same_resource_does_reap() {
    // The control for the refusals above: only the grace differs.
    let (s, id) = lapsed();
    let mut called = false;
    let reaped = s
        .guarded_reap(Some(&id), 4242, 0, "test", || {
            called = true;
            Ok(true)
        })
        .unwrap();
    assert!(reaped);
    assert!(called);
    assert_ne!(s.resource(&ResourceRef::Id(id)).unwrap().status, "lapsed");
}

/// `guarded_reap` checks the grace again inside its transaction: a resource that lapsed less
/// than `grace` ago is refused as no longer eligible, and its process is never stopped. (The
/// CLI plan also checks it, but a confirm can arrive after the plan; this is the last guard
/// before a process is killed.)
#[test]
fn a_reap_inside_the_grace_period_is_refused_without_calling_stop() {
    let (s, id) = lapsed();
    let mut called = false;
    let err = s
        .guarded_reap(Some(&id), 4242, 600, "test", || {
            called = true;
            Ok(true)
        })
        .unwrap_err();
    assert_eq!(err.code(), "conflict", "{err}");
    assert!(!called, "stop ran inside the grace period");
    assert_eq!(s.resource(&ResourceRef::Id(id)).unwrap().status, "lapsed");
}
