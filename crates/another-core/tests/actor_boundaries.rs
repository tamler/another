//! PR #296 review: the actor proof is a typed value, never text in a name field, and a
//! plain name mutates only on the explicit operator repair handle.

use another_core::test_support::wire_events_of;
use another_core::*;
use rusqlite::Connection;
use serde_json::Value;

fn act(store: &Store, actor: &ActorProof, id: &str, action: CompatibilityAction) -> Result<Value> {
    Ok(store.agent_contract(AgentEnvelope {
        major: 1,
        minor: 0,
        request_id: id.into(),
        required_features: vec![],
        operation: AgentOperation::Compatibility {
            actor: actor.clone(),
            action,
        },
    })?["result"]
        .clone())
}

fn forbidden_no_actor(result: Result<impl Sized>) {
    match result {
        Err(Error::Forbidden(m)) => assert!(m.starts_with("NO_ACTOR"), "{m}"),
        Err(e) => panic!("expected NO_ACTOR, got {e}"),
        Ok(_) => panic!("expected NO_ACTOR, the call succeeded"),
    }
}

fn register(store: &Store, name: &str, project: &str) {
    store
        .register_agent(RegisterAgent {
            name: name.into(),
            program: Program::Other,
            project: Some(project.into()),
            model: None,
            description: None,
        })
        .unwrap();
}

#[test]
fn a_never_minted_name_cannot_mutate_on_a_guarded_handle() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().to_string_lossy().into_owned();
    let store = Store::open_in_memory(Clock::System).unwrap();
    register(&store, "alice", &project);
    register(&store, "bob", &project);
    let hold = || AcquireHolds {
        agent: "alice".into(),
        project: project.clone(),
        paths: vec!["src/a.rs".into()],
        ttl_seconds: None,
        exclusive: true,
        reason: None,
        repo_wide: false,
    };
    forbidden_no_actor(store.acquire_holds(hold()));
    forbidden_no_actor(store.send_message(SendMessage {
        from: "alice".into(),
        to: vec!["bob".into()],
        subject: "s".into(),
        body: "b".into(),
        ..Default::default()
    }));
    forbidden_no_actor(store.queue_next("alice"));
    forbidden_no_actor(store.register_resource(RegisterResource {
        agent: "alice".into(),
        name: "dev".into(),
        kind: ResourceKind::DevServer,
        endpoint: None,
        ttl_seconds: None,
        project: None,
        port: None,
        pid: None,
        purpose: None,
        mode: LeaseMode::Shared,
        metadata: None,
    }));
    assert!(store.holds(HoldFilter::default()).unwrap().is_empty());
    // The operator's explicit repair handle is the only plain-name path.
    let repair = store.unguarded_repair();
    let granted = repair.acquire_holds(hold()).unwrap();
    assert_eq!(granted.granted.len(), 1);
    // The repair handle shares the database; the guarded handle still refuses.
    forbidden_no_actor(store.release_holds("alice", &[granted.granted[0].hold_id.clone()]));
}

#[test]
fn a_minted_actor_name_or_id_is_not_authority_even_on_the_repair_handle() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().to_string_lossy().into_owned();
    let store = Store::open_in_memory(Clock::System).unwrap();
    store.ensure_project("operator", &project).unwrap();
    let participant = store.enroll_participant().unwrap();
    let actor = store
        .register_actor(&participant, "root", "codex", None)
        .unwrap();
    let repair = store.unguarded_repair();
    for who in [actor.actor.name.as_str(), actor.actor.actor_id.as_str()] {
        forbidden_no_actor(repair.acquire_holds(AcquireHolds {
            agent: who.into(),
            project: project.clone(),
            paths: vec!["a".into()],
            ttl_seconds: None,
            exclusive: true,
            reason: None,
            repo_wide: false,
        }));
    }
}

/// The stated repair boundary (docs/AGENT-CONTRACT.md): repair holds and edit-admission
/// file leases are separate records that do not check each other, and only a
/// compatibility hold shares the holds table with repair holds.
#[test]
fn repair_holds_meet_only_compatibility_holds_not_edit_admission_leases() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(work.join("src")).unwrap();
    let project = work.to_string_lossy().into_owned();
    let store = Store::open_in_memory(Clock::System).unwrap();
    register(&store, "legacy", &project);
    let participant = store.enroll_participant().unwrap();
    let actor = store
        .register_actor(&participant, "root", "codex", None)
        .unwrap();
    let repair = store.unguarded_repair();
    let hold = |path: &str| AcquireHolds {
        agent: "legacy".into(),
        project: project.clone(),
        paths: vec![path.into()],
        ttl_seconds: Some(600),
        exclusive: true,
        reason: None,
        repo_wide: false,
    };
    assert_eq!(repair.acquire_holds(hold("src/a.rs")).unwrap().granted.len(), 1);
    let file = |name: &str| work.join("src").join(name).to_string_lossy().into_owned();
    store
        .begin_actor_edit(&actor.proof, "edit-a", &[file("a.rs")], None)
        .expect("edit admission does not consult repair holds");
    store
        .begin_actor_edit(&actor.proof, "edit-b", &[file("b.rs")], None)
        .unwrap();
    assert_eq!(
        repair.acquire_holds(hold("src/b.rs")).unwrap().granted.len(),
        1,
        "a repair hold does not consult edit-admission leases"
    );
    let compat = act(
        &store,
        &actor.proof,
        "compat-hold",
        CompatibilityAction::Hold {
            project: project.clone(),
            paths: vec!["src/a.rs".into()],
            ttl_seconds: Some(600),
        },
    );
    let granted = compat
        .as_ref()
        .map(|v| v["projection"]["granted"].as_array().map_or(0, Vec::len))
        .unwrap_or(0);
    assert_eq!(granted, 0, "a compatibility hold conflicts with a repair hold");
}

#[test]
fn the_actor_proof_never_reaches_events_errors_wire_events_or_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let project = work.to_string_lossy().into_owned();
    let store = Store::open(&home).unwrap();
    store.ensure_project("operator", &project).unwrap();
    register(&store, "bob", &project);
    let participant = store.enroll_participant().unwrap();
    let actor = store
        .register_actor(&participant, "root", "codex", None)
        .unwrap();
    let proof = &actor.proof;
    let mut errors = Vec::new();
    let held = act(
        &store,
        proof,
        "hold",
        CompatibilityAction::Hold {
            project: project.clone(),
            paths: vec!["src/a.rs".into()],
            ttl_seconds: Some(600),
        },
    )
    .unwrap();
    assert!(held["projection"]["granted"][0]["hold_id"].is_string());
    act(
        &store,
        proof,
        "send",
        CompatibilityAction::Send {
            to: vec!["bob".into()],
            subject: "hello".into(),
            body: "body".into(),
            create_request: true,
            due_in_seconds: None,
        },
    )
    .unwrap();
    for (id, action) in [
        (
            "bad-release",
            CompatibilityAction::ReleaseHolds {
                hold_ids: vec!["no-such-hold".into()],
            },
        ),
        (
            "bad-send",
            CompatibilityAction::Send {
                to: vec!["nobody-registered".into()],
                subject: "s".into(),
                body: "b".into(),
                create_request: false,
                due_in_seconds: None,
            },
        ),
        (
            "bad-borrow",
            CompatibilityAction::Borrow {
                resource_id: "no-such-resource".into(),
                ttl_seconds: None,
            },
        ),
    ] {
        match act(&store, proof, id, action) {
            Err(e) => errors.push(format!("{e} {e:?}")),
            Ok(v) => panic!("{id} unexpectedly succeeded: {v}"),
        }
    }
    let encoded = another_core::crypto::b64(&serde_json::to_vec(proof).unwrap());
    let needles = [
        proof.secret.as_str(),
        encoded.as_str(),
        "another-private:",
        participant.secret.as_str(),
    ];
    let events = serde_json::to_string(&store.events(10_000).unwrap()).unwrap();
    let conn = Connection::open(store.db_path().unwrap()).unwrap();
    let wire = serde_json::to_string(&wire_events_of(&conn, store.node_id())).unwrap();
    drop(conn);
    let mut on_disk = Vec::new();
    for entry in std::fs::read_dir(&home).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            on_disk.extend(std::fs::read(&path).unwrap());
        }
    }
    let on_disk = String::from_utf8_lossy(&on_disk);
    for needle in needles {
        assert!(!events.contains(needle), "event log carries {needle:?}");
        assert!(!wire.contains(needle), "federation wire events carry {needle:?}");
        assert!(!on_disk.contains(needle), "the home's files carry {needle:?}");
        for error in &errors {
            assert!(!error.contains(needle), "error carries {needle:?}: {error}");
        }
    }
    // The event log names the actor by its public id or label only.
    assert!(events.contains(&actor.actor.actor_id) || events.contains(&actor.actor.name));
}

/// PR #296 review P1: borrowing another actor's resource through the contract works and
/// no error carries a proof.
#[test]
fn an_actor_borrows_another_actors_resource_through_the_contract() {
    let s = Store::open_in_memory(Clock::System).unwrap();
    let p = s.enroll_participant().unwrap();
    let a = s.register_actor(&p, "a", "a", None).unwrap();
    let b = s.register_actor(&p, "b", "b", None).unwrap();
    let resource = act(
        &s,
        &a.proof,
        "resource",
        CompatibilityAction::RegisterResource {
            name: "shared-worker".into(),
            kind: ResourceKind::LongJob,
            endpoint: None,
            ttl_seconds: Some(600),
        },
    )
    .unwrap();
    let rid = resource["projection"]["resource"]["resource_id"]
        .as_str()
        .unwrap()
        .to_string();
    let borrowed = act(
        &s,
        &b.proof,
        "borrow",
        CompatibilityAction::Borrow {
            resource_id: rid.clone(),
            ttl_seconds: Some(600),
        },
    );
    let borrowed = match borrowed {
        Ok(v) => v,
        Err(e) => {
            let text = e.to_string();
            assert!(!text.contains(&b.proof.secret), "error carries the proof");
            panic!("borrow through the contract failed: {}", e.code());
        }
    };
    assert!(borrowed["leases"]
        .as_array()
        .unwrap()
        .iter()
        .any(|l| l["subject_kind"] == "resource" && l["released_at"].is_null()));
    assert!(s
        .resource(&ResourceRef::Id(rid))
        .unwrap()
        .leases
        .iter()
        .any(|l| l.agent == b.actor.name));
}

/// PR #296 review P2: the worker completes the creator's request through the contract;
/// the completion notice is sent as the authenticated worker, not refused as a name.
#[test]
fn a_worker_actor_completes_another_actors_request() {
    let s = Store::open_in_memory(Clock::System).unwrap();
    let p = s.enroll_participant().unwrap();
    let a = s.register_actor(&p, "a", "a", None).unwrap();
    let b = s.register_actor(&p, "b", "b", None).unwrap();
    let message = act(
        &s,
        &a.proof,
        "send",
        CompatibilityAction::Send {
            to: vec![b.actor.name.clone()],
            subject: "do work".into(),
            body: "request".into(),
            create_request: true,
            due_in_seconds: None,
        },
    )
    .unwrap();
    let rid = message["projection"]["request_id"]
        .as_str()
        .unwrap()
        .to_string();
    let claim = act(
        &s,
        &b.proof,
        "claim",
        CompatibilityAction::ClaimRequest {
            request_id: rid.clone(),
        },
    )
    .unwrap();
    let lease: RightLease = serde_json::from_value(
        claim["leases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["subject_kind"] == "request")
            .unwrap()
            .clone(),
    )
    .unwrap();
    act(
        &s,
        &b.proof,
        "complete",
        CompatibilityAction::Request {
            request_id: rid.clone(),
            request_action: RequestAction::Complete,
            ttl_seconds: None,
            lease: Some(LeaseRef::from(&lease)),
            note: None,
        },
    )
    .unwrap();
    assert_eq!(s.request(&rid).unwrap().status, RequestStatus::Done);
    let inbox = s.inbox(&a.actor.name, InboxQuery::default()).unwrap();
    assert!(
        inbox
            .messages
            .iter()
            .any(|m| m.message.subject.starts_with("Done:")),
        "the creator gets the completion notice"
    );
}

/// PR #296 review P2: mail from a minted actor on a spoke is delivered on the hub; the
/// actor's public directory entry travels with federation sync.
#[test]
fn a_minted_actors_mail_is_delivered_across_federation() {
    let hub = Store::open_in_memory(Clock::System).unwrap();
    let spoke = Store::open_in_memory(Clock::System).unwrap();
    hub.set_node_name("hub").unwrap();
    spoke.set_node_name("spoke").unwrap();
    let code = hub
        .create_invite("https://hub.example", &Perm::ALL, 3600)
        .unwrap();
    let (invite, req) = spoke.prepare_join(&code).unwrap();
    let resp = hub.accept_pairing(&req).unwrap();
    spoke.complete_join(&invite, &resp).unwrap();
    hub.register_agent(RegisterAgent {
        name: "owner".into(),
        program: Program::Other,
        project: None,
        model: None,
        description: None,
    })
    .unwrap();
    let page = hub.pull_for(spoke.node_id(), 0, 10_000).unwrap();
    let imported = spoke.ingest(hub.node_id(), &page.events, &page.nodes).unwrap();
    assert!(imported.rejected.is_empty());
    let grant = spoke.enroll_participant().unwrap();
    let actor = spoke
        .register_actor(&grant, "spoke-root", "worker", None)
        .unwrap();
    act(
        &spoke,
        &actor.proof,
        "remote-mail",
        CompatibilityAction::Send {
            to: vec!["owner@@hub".into()],
            subject: "remote work".into(),
            body: "hello".into(),
            create_request: false,
            due_in_seconds: None,
        },
    )
    .unwrap();
    let (events, _) = spoke.outbound_events(0, 10_000).unwrap();
    let report = hub
        .ingest(spoke.node_id(), &events, &[spoke.node_entry().unwrap()])
        .unwrap();
    assert!(report.rejected.is_empty());
    let inbox = hub.inbox("owner", InboxQuery::default()).unwrap();
    assert_eq!(inbox.messages.len(), 1, "the hub recipient got the actor's mail");
    // The directory entry is the public id only; no proof travels.
    let wire = serde_json::to_string(&events).unwrap();
    assert!(!wire.contains(&actor.proof.secret));
}

#[test]
fn a_relative_file_subject_is_refused_not_resolved_from_the_serving_process() {
    let store = Store::open_in_memory(Clock::System).unwrap();
    let participant = store.enroll_participant().unwrap();
    let actor = store
        .register_actor(&participant, "root", "codex", None)
        .unwrap();
    for refused in [
        store
            .checkout_right(
                &actor.proof,
                "relative",
                CheckoutRight {
                    subject_kind: SubjectKind::File,
                    subject_key: "src/a.rs".into(),
                    ttl_seconds: None,
                },
            )
            .map(|_| ()),
        store
            .begin_actor_edit(&actor.proof, "edit", &["src/a.rs".into()], None)
            .map(|_| ()),
    ] {
        match refused {
            Err(Error::Invalid(m)) => assert!(m.contains("absolute"), "{m}"),
            other => panic!("expected a refusal, got {:?}", other.err()),
        }
    }
    assert!(store.right_leases().unwrap().is_empty());
}
