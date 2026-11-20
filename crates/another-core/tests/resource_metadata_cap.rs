//! Resource metadata is capped: an oversized blob is refused before anything is written.

use another_core::*;
use serde_json::{json, Value};

const T0: i64 = 1_800_000_000_000;

fn store() -> Store {
    let (clock, _handle) = Clock::manual(T0);
    let store = Store::open_in_memory(clock).unwrap();
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

fn register(store: &Store, metadata: Value) -> Result<LeaseOutcome> {
    store.register_resource(RegisterResource {
        agent: "alice".into(),
        kind: ResourceKind::ModelServer,
        name: "ollama@11434".into(),
        endpoint: Some("http://127.0.0.1:11434".into()),
        port: None,
        pid: None,
        project: None,
        purpose: None,
        mode: LeaseMode::Shared,
        ttl_seconds: Some(3_600),
        metadata: Some(metadata),
    })
}

fn resources(store: &Store) -> usize {
    store
        .find_resources(ResourceFilter {
            include_released: true,
            ..Default::default()
        })
        .unwrap()
        .len()
}

/// `{"blob":"x…x"}` whose serialized form is exactly `len` bytes.
fn blob_of_serialized_len(len: usize) -> Value {
    let overhead = json!({"blob": ""}).to_string().len();
    let value = json!({"blob": "x".repeat(len - overhead)});
    assert_eq!(value.to_string().len(), len);
    value
}

#[test]
fn oversized_metadata_is_refused_before_anything_is_written() {
    let store = store();
    let events_before = store.events(10_000).unwrap().len();

    let err = register(&store, json!({"blob": "x".repeat(1024 * 1024)}))
        .expect_err("1 MiB of metadata was accepted");
    assert_eq!(err.code(), "invalid", "{err}");
    assert!(err.to_string().contains("metadata"), "{err}");

    assert_eq!(resources(&store), 0, "a resource was stored");
    assert_eq!(
        store.events(10_000).unwrap().len(),
        events_before,
        "an event was written"
    );
}

#[test]
fn metadata_at_the_cap_is_accepted_and_one_byte_over_is_refused() {
    let store = store();

    let over = register(
        &store,
        blob_of_serialized_len(MAX_RESOURCE_METADATA_BYTES + 1),
    )
    .expect_err("metadata one byte over the cap was accepted");
    assert_eq!(over.code(), "invalid", "{over}");
    assert_eq!(resources(&store), 0);

    register(&store, blob_of_serialized_len(MAX_RESOURCE_METADATA_BYTES))
        .expect("metadata exactly at the cap was refused");
    assert_eq!(resources(&store), 1);
}

#[test]
fn ordinary_metadata_is_unaffected() {
    let store = store();
    register(
        &store,
        json!({"model": "llama3", "backend": "metal", "estimated_bytes": 4_000_000_000u64}),
    )
    .unwrap();
    assert_eq!(resources(&store), 1);
}
