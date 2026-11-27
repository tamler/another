//! A resource's endpoint and purpose are size limited (queue item 66,
//! FINDINGS-main-resource-text-size-w9W): both are stored, written into
//! `resource.registered` and synced to peers. A value at the limit is accepted; one byte
//! more is refused as `invalid` before anything is written, on a first registration and on
//! a re-registration that adopts a lapsed resource.

use another_core::*;

fn store() -> Store {
    let store = Store::open_in_memory(Clock::manual(1_800_000_000_000).0)
        .unwrap()
        .unguarded_repair();
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

fn register(
    store: &Store,
    name: &str,
    endpoint: Option<String>,
    purpose: Option<String>,
) -> Result<LeaseOutcome> {
    store.register_resource(RegisterResource {
        agent: "alice".into(),
        kind: ResourceKind::DevServer,
        name: name.into(),
        endpoint,
        port: Some(5173),
        pid: None,
        project: None,
        purpose,
        mode: LeaseMode::Shared,
        ttl_seconds: None,
        metadata: None,
    })
}

/// An endpoint of exactly `len` bytes.
fn endpoint_of(len: usize) -> String {
    let head = "http://127.0.0.1:5173/";
    format!("{head}{}", "a".repeat(len - head.len()))
}

#[test]
fn an_endpoint_or_purpose_at_the_limit_is_accepted() {
    let s = store();
    let endpoint = endpoint_of(MAX_RESOURCE_ENDPOINT_BYTES);
    let purpose = "p".repeat(MAX_RESOURCE_PURPOSE_BYTES);
    let out = register(
        &s,
        "at-limit",
        Some(endpoint.clone()),
        Some(purpose.clone()),
    )
    .unwrap();
    assert_eq!(out.resource.endpoint.as_deref(), Some(endpoint.as_str()));
    assert_eq!(out.resource.purpose.as_deref(), Some(purpose.as_str()));
}

#[test]
fn one_byte_over_or_a_megabyte_is_refused_and_nothing_is_written() {
    let s = store();
    let before = s.events(10_000).unwrap().len();
    let big = "x".repeat(1024 * 1024);
    for (case, endpoint, purpose, field) in [
        (
            "endpoint+1",
            Some(endpoint_of(MAX_RESOURCE_ENDPOINT_BYTES + 1)),
            None,
            "endpoint",
        ),
        (
            "purpose+1",
            None,
            Some("p".repeat(MAX_RESOURCE_PURPOSE_BYTES + 1)),
            "purpose",
        ),
        (
            "1 MiB endpoint",
            Some(format!("http://127.0.0.1:1/{big}")),
            None,
            "endpoint",
        ),
        ("1 MiB purpose", None, Some(big.clone()), "purpose"),
    ] {
        let err = register(&s, "too-big", endpoint, purpose).unwrap_err();
        assert_eq!(err.code(), "invalid", "{case}: {err}");
        assert!(
            err.to_string().contains(&format!("resource {field} is")),
            "{case}: {err}"
        );
    }
    let all = s
        .find_resources(ResourceFilter {
            include_released: true,
            ..Default::default()
        })
        .unwrap();
    assert!(all.is_empty(), "a refused registration stored a resource");
    assert_eq!(
        s.events(10_000).unwrap().len(),
        before,
        "a refusal wrote an event"
    );
}

#[test]
fn re_registering_a_lapsed_resource_cannot_grow_its_endpoint() {
    let s = store();
    let id = register(&s, "lapsing", Some("http://127.0.0.1:5173".into()), None)
        .unwrap()
        .resource
        .resource_id;
    s.release_resource("alice", &ResourceRef::Id(id.clone()), false)
        .unwrap();
    let err = register(
        &s,
        "lapsing",
        Some(endpoint_of(MAX_RESOURCE_ENDPOINT_BYTES + 1)),
        None,
    )
    .unwrap_err();
    assert_eq!(err.code(), "invalid", "{err}");
    assert_eq!(
        s.resource(&ResourceRef::Id(id))
            .unwrap()
            .endpoint
            .as_deref(),
        Some("http://127.0.0.1:5173")
    );
}
