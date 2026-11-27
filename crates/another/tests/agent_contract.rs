use std::io::Write;
use std::process::{Command, Stdio};

use another_core::{
    AgentEnvelope, AgentOperation, CheckoutRight, Clock, RegisteredActor, Store, SubjectKind,
};
use serde_json::Value;

fn event(home: &std::path::Path, request: &AgentEnvelope) -> (bool, Value) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["--json", "adapter", "event"])
        .env("ANOTHER_HOME", home)
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let raw = serde_json::to_vec(request).unwrap();
    child.stdin.take().unwrap().write_all(&raw).unwrap();
    let output = child.wait_with_output().unwrap();
    (
        output.status.success(),
        serde_json::from_slice(&output.stdout).unwrap(),
    )
}

#[test]
fn private_stdin_registration_checkout_and_public_status() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_with_clock(dir.path(), Clock::System).unwrap();
    let participant = store.enroll_participant().unwrap();
    let mut request = AgentEnvelope {
        major: 1,
        minor: 0,
        request_id: "register".into(),
        required_features: vec!["leases".into()],
        operation: AgentOperation::Register {
            participant,
            native_key: "native-root".into(),
            label: "codex".into(),
            parent: None,
        },
    };
    let (success, response) = event(dir.path(), &request);
    assert!(success);
    let registered: RegisteredActor = serde_json::from_value(response["result"].clone()).unwrap();
    request.request_id = "checkout".into();
    request.operation = AgentOperation::Checkout {
        actor: registered.proof.clone(),
        checkout: CheckoutRight {
            subject_kind: SubjectKind::File,
            subject_key: "canonical-checkout/A".into(),
            ttl_seconds: None,
        },
    };
    let (success, first) = event(dir.path(), &request);
    assert!(success);
    assert_eq!(first["result"]["ttl_seconds"], 120);
    let (success, retry) = event(dir.path(), &request);
    assert!(success);
    assert_eq!(first["result"], retry["result"]);
    let before = store.events(500).unwrap().len();
    let status = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["adapter", "status"])
        .env("ANOTHER_HOME", dir.path())
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(status.status.success());
    let status = String::from_utf8(status.stdout).unwrap();
    assert!(!status.contains(&registered.proof.secret));
    assert!(!status.contains("proof_hash"));
    assert_eq!(store.events(500).unwrap().len(), before);
    request.major = 2;
    let (success, response) = event(dir.path(), &request);
    assert!(!success);
    assert!(response["error"]["message"]
        .as_str()
        .unwrap()
        .contains("INCOMPATIBLE_VERSION"));
    assert_eq!(store.events(500).unwrap().len(), before);
}

#[test]
fn repository_status_uses_file_evidence_and_is_pure() {
    let dir = tempfile::tempdir().unwrap();
    let inside = dir.path().join("inside");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&inside).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let home = dir.path().join("state");
    let store = Store::open_with_clock(&home, Clock::System).unwrap();
    let grant = store.enroll_participant().unwrap();
    let root = store.register_actor(&grant, "root", "codex", None).unwrap();
    let child = store
        .register_actor(&grant, "child", "child", Some(&root.proof))
        .unwrap();
    let other = store
        .register_actor(&grant, "other", "other", None)
        .unwrap();
    for (actor, path) in [(&child, inside.join("A")), (&other, outside.join("B"))] {
        store
            .begin_actor_edit(
                &actor.proof,
                "edit",
                &[path.to_string_lossy().into_owned()],
                None,
            )
            .unwrap();
    }
    let before = store.events(1000).unwrap().len();
    let out = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["--json", "status", "--repo"])
        .arg(&inside)
        .env("ANOTHER_HOME", &home)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    let actors = value["actors"].as_array().unwrap();
    assert_eq!(actors.len(), 2);
    assert!(actors.iter().any(|a| a["actor_id"] == root.actor.actor_id));
    assert!(actors.iter().any(|a| a["actor_id"] == child.actor.actor_id));
    assert_eq!(value["leases"].as_array().unwrap().len(), 1);
    assert_eq!(value["operations"].as_array().unwrap().len(), 1);
    assert!(!String::from_utf8_lossy(&out.stdout).contains(&child.proof.secret));
    assert_eq!(store.events(1000).unwrap().len(), before);
}

#[test]
fn public_status_does_not_create_a_missing_home() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing-home");
    let output = Command::new(env!("CARGO_BIN_EXE_another"))
        .args(["--json", "adapter", "status"])
        .env("ANOTHER_HOME", &missing)
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!missing.exists());
}
