use std::sync::atomic::Ordering;

use another_core::*;
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

#[test]
fn session_end_releases_file_resource_queue_request_and_preserves_history() {
    let dir = tempfile::tempdir().unwrap();
    let (clock, _) = Clock::manual(1_800_000_000_000);
    let store = Store::open_in_memory(clock).unwrap();
    let participant = store.enroll_participant().unwrap();
    let root = store
        .register_actor(&participant, "root", "root", None)
        .unwrap();
    let child = store
        .register_actor(&participant, "child", "child", Some(&root.proof))
        .unwrap();
    let project = dir.path().to_string_lossy().into_owned();
    store.ensure_project("operator", &project).unwrap();
    let held = act(
        &store,
        &child.proof,
        "hold",
        CompatibilityAction::Hold {
            project,
            paths: vec!["A".into()],
            ttl_seconds: Some(600),
        },
    )
    .unwrap();
    let hold_id = held["projection"]["granted"][0]["hold_id"]
        .as_str()
        .unwrap()
        .to_string();
    let resource = act(
        &store,
        &child.proof,
        "resource",
        CompatibilityAction::RegisterResource {
            name: "worker".into(),
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
    let item = store
        .queue_propose(
            &QueueCaller::Owner,
            ProposeItem {
                title: "work".into(),
                scope: Some("M2".into()),
                ..Default::default()
            },
        )
        .unwrap();
    store
        .queue_admit(
            &QueueCaller::Owner,
            &item.item_id,
            AdmitItem {
                done_when: Some("verified result".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(act(
        &store,
        &child.proof,
        "claim",
        CompatibilityAction::Next {
            ttl_seconds: Some(600)
        }
    )
    .unwrap()["projection"]["claimed"]
        .is_object());
    // Pre-contract senders act only through the explicit operator repair handle.
    let repair = store.unguarded_repair();
    let message = repair.send_message(SendMessage {
        from: "operator".into(),
        to: vec![child.actor.name.clone()],
        subject: "request".into(),
        request: Some(NewRequest::default()),
        ..Default::default()
    });
    // A legacy public sender must first be explicitly registered; it cannot become a
    // minted actor or acquire that actor's rights by choosing the same display name.
    assert!(message.is_err());
    store
        .register_agent(RegisterAgent {
            name: "operator".into(),
            program: Program::Other,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    let message = repair
        .send_message(SendMessage {
            from: "operator".into(),
            to: vec![child.actor.name.clone()],
            subject: "request".into(),
            request: Some(NewRequest::default()),
            ..Default::default()
        })
        .unwrap();
    let request = message.request_id.clone().unwrap();
    act(
        &store,
        &child.proof,
        "request",
        CompatibilityAction::ClaimRequest {
            request_id: request.clone(),
        },
    )
    .unwrap();
    for handle in [&store, &repair] {
        assert!(
            handle
                .renew_holds(&child.actor.name, std::slice::from_ref(&hold_id))
                .is_err(),
            "public name is not authority, even on the repair handle"
        );
    }
    let before_end: std::collections::BTreeSet<_> = store
        .right_leases()
        .unwrap()
        .into_iter()
        .filter(|l| l.released_at.is_none())
        .map(|l| l.lease_id)
        .collect();
    let ended: std::collections::BTreeSet<_> = store
        .end_actor_session(&root.proof, "end")
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(
        ended, before_end,
        "session end returns every released projection id"
    );
    for lease in store
        .right_leases()
        .unwrap()
        .into_iter()
        .filter(|l| before_end.contains(&l.lease_id))
    {
        assert_eq!(lease.release_reason.as_deref(), Some("session_end"));
    }
    assert!(store.holds(HoldFilter::default()).unwrap().is_empty());
    assert!(store
        .resource(&ResourceRef::Id(rid))
        .unwrap()
        .leases
        .is_empty());
    assert_eq!(
        store.queue_item(&item.item_id).unwrap().state,
        QueueState::Ready
    );
    assert_eq!(store.request(&request).unwrap().status, RequestStatus::Open);
    assert!(!store
        .thread(&message.thread_id)
        .unwrap()
        .messages
        .is_empty());
    assert!(store
        .right_leases()
        .unwrap()
        .iter()
        .all(|l| l.released_at.is_some()));
}

#[test]
fn common_expiry_returns_queue_claim_without_grace_or_own_caller_exemption() {
    let (clock, now) = Clock::manual(1_800_000_000_000);
    let store = Store::open_in_memory(clock).unwrap();
    let participant = store.enroll_participant().unwrap();
    let actor = store
        .register_actor(&participant, "root", "root", None)
        .unwrap();
    let item = store
        .queue_propose(
            &QueueCaller::Owner,
            ProposeItem {
                title: "work".into(),
                scope: Some("M2".into()),
                ..Default::default()
            },
        )
        .unwrap();
    store
        .queue_admit(
            &QueueCaller::Owner,
            &item.item_id,
            AdmitItem {
                done_when: Some("verified result".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let first = act(
        &store,
        &actor.proof,
        "claim",
        CompatibilityAction::Next {
            ttl_seconds: Some(30),
        },
    )
    .unwrap();
    assert_eq!(
        act(
            &store,
            &actor.proof,
            "claim",
            CompatibilityAction::Next {
                ttl_seconds: Some(30)
            }
        )
        .unwrap(),
        first
    );
    now.fetch_add(30_000, Ordering::SeqCst);
    let second = act(
        &store,
        &actor.proof,
        "fresh",
        CompatibilityAction::Next {
            ttl_seconds: Some(600),
        },
    )
    .unwrap();
    assert!(second["projection"]["claimed"].is_object());
    assert!(second["leases"]
        .as_array()
        .unwrap()
        .iter()
        .any(|l| l["ttl_seconds"] == 600 && l["generation"] == 2));
}
