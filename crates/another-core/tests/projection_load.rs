//! Load test for `sync_projections`, which runs twice inside every write transaction.
//! It builds a home with a growing history of ended projected rights (holds, resource
//! leases, queue claims and request claims, each taken and released through the contract)
//! and measures the median time of one cheap write at each size.
//!
//! Ignored by default because it takes tens of seconds in a debug build. Run it with
//! `cargo test -p another-core --release --test projection_load -- --ignored --nocapture`
//! to print the table. The assertion is a coarse guard against cost that grows with
//! history: the median write at the largest size stays within a few times the smallest.

use std::time::{Duration, Instant};

use another_core::*;
use serde_json::Value;

fn act(store: &Store, actor: &ActorProof, id: &str, action: CompatibilityAction) -> Value {
    store
        .agent_contract(AgentEnvelope {
            major: 1,
            minor: 0,
            request_id: id.into(),
            required_features: vec![],
            operation: AgentOperation::Compatibility {
                actor: actor.clone(),
                action,
            },
        })
        .unwrap_or_else(|e| panic!("{id}: {e}"))["result"]
        .clone()
}

fn lease(v: &Value, kind: &str) -> LeaseRef {
    let l = v["leases"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|l| l["subject_kind"] == kind && l["released_at"].is_null())
        .unwrap_or_else(|| panic!("no live {kind} lease in {v}"));
    serde_json::from_value(serde_json::json!({
        "lease_id": l["lease_id"], "generation": l["generation"]
    }))
    .unwrap()
}

/// One round of history: a hold, a resource borrow, a queue claim finished and a request
/// claim, each through the contract, and each ended.
fn round(store: &Store, actor: &ActorProof, recipient: &str, project: &str, i: usize) {
    let held = act(
        store,
        actor,
        &format!("hold-{i}"),
        CompatibilityAction::Hold {
            project: project.into(),
            paths: vec![format!("src/f{i}.rs")],
            ttl_seconds: Some(600),
        },
    );
    let hold_id = held["projection"]["granted"][0]["hold_id"]
        .as_str()
        .unwrap()
        .to_string();
    act(
        store,
        actor,
        &format!("unhold-{i}"),
        CompatibilityAction::ReleaseHolds {
            hold_ids: vec![hold_id],
        },
    );
    let registered = act(
        store,
        actor,
        &format!("res-{i}"),
        CompatibilityAction::RegisterResource {
            name: format!("job-{i}"),
            kind: ResourceKind::LongJob,
            endpoint: None,
            ttl_seconds: Some(600),
        },
    );
    let rid = registered["projection"]["resource"]["resource_id"]
        .as_str()
        .unwrap()
        .to_string();
    act(
        store,
        actor,
        &format!("unres-{i}"),
        CompatibilityAction::ReleaseResource {
            resource_id: rid,
            lease: lease(&registered, "resource"),
        },
    );
    let item = store
        .queue_propose(
            &QueueCaller::Owner,
            ProposeItem {
                title: format!("work {i}"),
                scope: Some("load".into()),
                ..Default::default()
            },
        )
        .unwrap();
    store
        .queue_admit(
            &QueueCaller::Owner,
            &item.item_id,
            AdmitItem {
                done_when: Some("done".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let claimed = act(
        store,
        actor,
        &format!("next-{i}"),
        CompatibilityAction::Next {
            ttl_seconds: Some(600),
        },
    );
    act(
        store,
        actor,
        &format!("done-{i}"),
        CompatibilityAction::Done {
            item: item.item_id,
            summary: "done".into(),
            commit: format!("{:040x}", i + 1),
            lease: lease(&claimed, "queue"),
        },
    );
    let sent = store
        .unguarded_repair()
        .send_message(SendMessage {
            from: "operator".into(),
            to: vec![recipient.into()],
            subject: format!("ask {i}"),
            request: Some(NewRequest::default()),
            ..Default::default()
        })
        .unwrap();
    let request_id = sent.request_id.unwrap();
    let claim = act(
        store,
        actor,
        &format!("claim-{i}"),
        CompatibilityAction::ClaimRequest {
            request_id: request_id.clone(),
        },
    );
    act(
        store,
        actor,
        &format!("complete-{i}"),
        CompatibilityAction::Request {
            request_id,
            request_action: RequestAction::Complete,
            ttl_seconds: None,
            lease: Some(lease(&claim, "request")),
            note: None,
        },
    );
}

fn median_write(store: &Store, project: &str) -> Duration {
    let mut times: Vec<Duration> = (0..41)
        .map(|_| {
            let start = Instant::now();
            store.ensure_project("operator", project).unwrap();
            start.elapsed()
        })
        .collect();
    times.sort();
    times[times.len() / 2]
}

#[test]
#[ignore = "load test; run with --ignored --nocapture"]
fn sync_projections_cost_does_not_grow_with_ended_history() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let project = work.to_string_lossy().into_owned();
    let store = Store::open(&dir.path().join("home")).unwrap();
    store.ensure_project("operator", &project).unwrap();
    store
        .register_agent(RegisterAgent {
            name: "operator".into(),
            program: Program::Other,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    let participant = store.enroll_participant().unwrap();
    let actor = store
        .register_actor(&participant, "root", "root", None)
        .unwrap();
    let mut rows = Vec::new();
    let mut done = 0;
    for target in [0usize, 250, 1000, 2500] {
        while done < target {
            round(&store, &actor.proof, &actor.actor.name, &project, done);
            done += 1;
        }
        let projections = store.right_leases().unwrap().len();
        rows.push((target, projections, median_write(&store, &project)));
    }
    println!("rounds  ended_leases  median_write");
    for (rounds, leases, t) in &rows {
        println!("{rounds:>6}  {leases:>12}  {t:>12.2?}");
    }
    let (first, last) = (rows[0].2, rows.last().unwrap().2);
    assert!(
        last <= first * 4 + Duration::from_millis(2),
        "a write costs {last:?} with history against {first:?} without"
    );
}
