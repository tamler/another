//! The activity feed's one-line text for decisions, spend, lanes and nodes, produced by doing the
//! real thing rather than by writing events. A person reads these lines, and they go to scoped
//! callers, so answers' free text, reasons and bodies must stay out of them.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use another_core::*;

const T0: i64 = 1_795_219_200_000; // 2026-11-21: well inside a monthly period
const HOUR: i64 = 3_600_000;

fn setup() -> (Store, Arc<AtomicI64>) {
    let (clock, handle) = Clock::manual(T0);
    let s = Store::open_in_memory(clock).unwrap();
    s.ensure_project("t", "/work/shop").unwrap();
    s.ensure_project("t", "/work/vault").unwrap();
    for (name, program, project) in [
        ("manager", Program::GrokBot, None),
        ("claude", Program::Claude, Some("/work/shop")),
        ("vaulter", Program::Claude, Some("/work/vault")),
    ] {
        s.register_agent_trusted(RegisterAgent {
            name: name.into(),
            program,
            project: project.map(Into::into),
            model: None,
            description: None,
        })
        .unwrap();
    }
    (s, handle)
}

fn feed(s: &Store) -> Vec<String> {
    s.activity(ActivityQuery {
        since: None,
        limit: 500,
        kinds: vec![],
    })
    .unwrap()
    .items
    .into_iter()
    .map(|i| i.summary)
    .collect()
}

#[track_caller]
fn has(lines: &[String], wanted: &str) {
    assert!(
        lines.iter().any(|l| l.contains(wanted)),
        "no line with {wanted:?} in:\n{}",
        lines.join("\n")
    );
}

fn ask(s: &Store, f: impl FnOnce(&mut NewDecision)) -> Decision {
    let mut input = NewDecision {
        agent: "claude@shop".into(),
        question: "The secret question?".into(),
        options: ["go", "wait"]
            .into_iter()
            .map(|v| DecisionOption {
                value: v.into(),
                label: v.into(),
                description: None,
            })
            .collect(),
        allow_free_text: true,
        ..Default::default()
    };
    f(&mut input);
    s.ask_decision(input, None).unwrap()
}

#[test]
fn decisions_read_plainly_and_keep_their_text_out() {
    let (s, clock) = setup();
    let now = s.now();

    // Answered with a value, relayed; answered with free text only.
    let a = ask(&s, |_| {});
    s.answer_decision(
        AnswerDecision {
            decision: a.decision_id.clone(),
            value: Some("go".into()),
            text: Some("the secret note".into()),
            answered_by: Some("pat".into()),
            relayed_by: Some("manager".into()),
        },
        None,
    )
    .unwrap();
    let b = ask(&s, |_| {});
    s.answer_decision(
        AnswerDecision {
            decision: b.decision_id.clone(),
            text: Some("the secret answer".into()),
            answered_by: Some("pat".into()),
            relayed_by: None,
            value: None,
        },
        None,
    )
    .unwrap();

    // Each way a deadline can go.
    let chosen = ask(&s, |d| {
        d.deadline_at = Some(now + HOUR);
        d.on_timeout = DecisionTimeout::ChooseDefault;
        d.default_value = Some("wait".into());
    });
    let waiting = ask(&s, |d| {
        d.deadline_at = Some(now + HOUR);
        d.on_timeout = DecisionTimeout::KeepOpen;
    });
    let lapsing = ask(&s, |d| {
        d.deadline_at = Some(now + HOUR);
        d.on_timeout = DecisionTimeout::Cancel;
    });
    let withdrawn = ask(&s, |_| {});
    s.cancel_decision(
        "claude@shop",
        &withdrawn.decision_id,
        Some("the secret reason".into()),
        None,
    )
    .unwrap();
    clock.fetch_add(HOUR + 1, Ordering::SeqCst);
    s.sweep().unwrap();

    let lines = feed(&s);
    has(&lines, "claude@shop asked for a decision");
    has(
        &lines,
        &format!(
            "pat answered decision {} with `go` (via manager)",
            a.decision_id
        ),
    );
    has(
        &lines,
        &format!("pat answered decision {} with free text", b.decision_id),
    );
    has(
        &lines,
        &format!(
            "decision {} timed out; default `wait` applies",
            chosen.decision_id
        ),
    );
    has(
        &lines,
        &format!(
            "decision {} is past its deadline, still waiting",
            waiting.decision_id
        ),
    );
    has(
        &lines,
        &format!(
            "decision {} timed out and was cancelled",
            lapsing.decision_id
        ),
    );
    has(
        &lines,
        &format!("withdrew decision {}", withdrawn.decision_id),
    );
    let all = lines.join("\n");
    assert!(!all.contains("secret"), "text leaked into the feed:\n{all}");
}

#[test]
fn spend_reads_plainly_through_an_accounts_life() {
    let (s, clock) = setup();
    let new = |name: &str, approval: bool, budget: Option<f64>| {
        s.create_spend_account(
            NewSpendAccount {
                name: name.into(),
                kind: SpendKind::GpuPod,
                unit: "USD".into(),
                budget,
                period: SpendPeriod::Monthly,
                reset_day: 1,
                cost_per_hour: Some(2.0),
                requires_approval: approval,
                approver: approval.then(|| "pat".to_string()),
                project: Some("shop".into()),
                description: None,
            },
            None,
        )
        .unwrap();
    };
    new("gpu", false, Some(10.0));
    new("big-gpu", true, None);
    s.update_spend_account(
        "gpu",
        SpendAccountUpdate {
            budget: Some(10.0),
            ..Default::default()
        },
        None,
    )
    .unwrap();

    // A recorded amount that crosses budget thresholds.
    s.record_spend(
        RecordSpend {
            agent: Some("claude@shop".into()),
            account: "gpu".into(),
            amount: 9.5,
            ..Default::default()
        },
        None,
    )
    .unwrap();

    // A lease, run for an hour.
    let lease = s
        .borrow_spend(
            BorrowSpend {
                agent: "claude@shop".into(),
                account: "gpu".into(),
                ttl_seconds: Some(7200),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    clock.fetch_add(HOUR, Ordering::SeqCst);
    s.release_spend("claude@shop", &lease.lease.lease_id, None)
        .unwrap();

    // Approval: requested and denied; requested and withdrawn.
    let denied = s
        .borrow_spend(
            BorrowSpend {
                agent: "claude@shop".into(),
                account: "big-gpu".into(),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    s.answer_decision(
        AnswerDecision {
            decision: denied.decision_id.clone().unwrap(),
            value: Some("deny".into()),
            relayed_by: Some("manager".into()),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let withdrawn = s
        .borrow_spend(
            BorrowSpend {
                agent: "claude@shop".into(),
                account: "big-gpu".into(),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    s.release_spend("claude@shop", &withdrawn.lease.lease_id, None)
        .unwrap();

    // Linking a resource, and unlinking it.
    let r = s
        .register_resource(RegisterResource {
            agent: "claude@shop".into(),
            kind: ResourceKind::Gpu,
            name: "metal:0".into(),
            endpoint: None,
            port: None,
            pid: None,
            project: Some("shop".into()),
            purpose: None,
            mode: LeaseMode::Shared,
            ttl_seconds: Some(600),
            metadata: None,
        })
        .unwrap()
        .resource;
    let target = ResourceRef::Id(r.resource_id.clone());
    s.link_resource_spend(&target, Some("gpu"), None).unwrap();
    s.link_resource_spend(&target, None, None).unwrap();
    s.archive_spend_account("gpu", None).unwrap();

    let lines = feed(&s);
    for wanted in [
        "spend account gpu added",
        "spend account gpu updated",
        "gpu reached 80% of its budget (9.5 / 10 USD)",
        "gpu reached 100% of its budget (11.5 / 10 USD)",
        &format!(
            "withdrew borrowing request {} on big-gpu",
            withdrawn.lease.lease_id
        ),
        "claude@shop spent 9.5 USD on gpu",
        "started using gpu at 2 USD/h",
        "lease on gpu ended (released): 2 USD",
        "(lease)",
        "asked to borrow big-gpu (approval needed)",
        &format!("borrowing request {} was denied", denied.lease.lease_id),
        &format!("resource {} linked to a spend account", r.resource_id),
        &format!("resource {} unlinked from its spend account", r.resource_id),
    ] {
        has(&lines, wanted);
    }
}

#[test]
fn feed_filters_and_cursors_keep_to_a_tokens_projects() {
    let (s, clock) = setup();
    let send = |from: &str, to: &str, subject: &str| {
        s.send_message(SendMessage {
            from: from.into(),
            to: vec![to.into()],
            subject: subject.into(),
            body: "body".into(),
            ..Default::default()
        })
        .unwrap();
    };
    send("claude@shop", "claude@shop", "shop mail");
    let first = s
        .activity(ActivityQuery {
            since: None,
            limit: 500,
            kinds: vec![ActivityKind::Mail],
        })
        .unwrap();
    assert!(first.items.iter().all(|i| i.category == ActivityKind::Mail));
    clock.fetch_add(1_000, Ordering::SeqCst);
    send("vaulter@vault", "vaulter@vault", "vault mail");
    clock.fetch_add(1_000, Ordering::SeqCst);

    let token = s
        .create_token("claude@shop", &["/work/shop".into()], None, None)
        .unwrap();
    let scope = s.verify_token(&token.secret).unwrap();
    let scoped = s
        .activity_scoped(
            ActivityQuery {
                since: Some(Since::parse("24h", s.now()).unwrap()),
                limit: 500,
                kinds: vec![],
            },
            Some(&scope),
        )
        .unwrap();
    let text: Vec<String> = scoped.items.iter().map(|i| i.summary.clone()).collect();
    has(&text, "shop mail");
    assert!(!text.iter().any(|l| l.contains("vault mail")), "{text:#?}");

    // Everything after the last visible event is hidden: the cursor doesn't name a hidden
    // event, and it's still usable.
    let cursor = scoped.next_since.clone().expect("a cursor");
    let vault_ids: Vec<String> = s
        .activity(ActivityQuery {
            since: None,
            limit: 500,
            kinds: vec![],
        })
        .unwrap()
        .items
        .into_iter()
        .filter(|i| i.summary.contains("vault"))
        .map(|i| i.event_id)
        .collect();
    assert!(!vault_ids.contains(&cursor), "{cursor}");
    let again = s
        .activity_scoped(
            ActivityQuery {
                since: Some(Since::parse(&cursor, s.now()).unwrap()),
                limit: 500,
                kinds: vec![],
            },
            Some(&scope),
        )
        .unwrap();
    assert!(
        !again.items.iter().any(|i| i.summary.contains("vault mail")),
        "{:?}",
        again.items
    );

    // A malformed `since` and an unknown kind are refused.
    assert!(Since::parse("whenever", s.now()).is_err());
    assert!("nonsense".parse::<ActivityKind>().is_err());
    for k in [
        "mail",
        "request",
        "hold",
        "resource",
        "session",
        "system",
        "federation",
        "lane",
        "spend",
    ] {
        let kind: ActivityKind = k.parse().unwrap();
        assert_eq!(kind.as_str(), k);
    }
}
