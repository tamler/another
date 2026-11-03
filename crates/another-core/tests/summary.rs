//! Daily summary: every section filled from a realistic day, the attention list, the phone
//! markdown, and token scoping (nothing from another project leaks into any section).

use another_core::*;

const NOV_21: i64 = 1_795_219_200_000;
const HOUR: i64 = 3_600_000;

fn opts() -> Vec<DecisionOption> {
    ["ship", "wait"]
        .iter()
        .map(|v| DecisionOption {
            value: v.to_string(),
            label: v.to_uppercase(),
            description: None,
        })
        .collect()
}

fn day() -> Store {
    let (clock, h) = Clock::manual(NOV_21);
    let s = Store::open_in_memory(clock).unwrap();
    for p in ["/work/shop", "/work/vault"] {
        s.ensure_project("t", p).unwrap();
    }
    for (name, project) in [
        ("manager", None),
        ("claude", Some("/work/shop")),
        ("codex", Some("/work/shop")),
        ("vaulter", Some("/work/vault")),
    ] {
        s.register_agent(RegisterAgent {
            name: name.into(),
            program: Program::Claude,
            project: project.map(Into::into),
            model: None,
            description: None,
        })
        .unwrap();
    }
    let lane = |name: &str, project: &str, lead: &str| {
        s.create_lane(
            NewLane {
                agent: "manager".into(),
                name: name.into(),
                project: project.into(),
                lead: lead.into(),
                charter: String::new(),
                status: Some(LaneStatus::Working),
            },
            None,
        )
        .unwrap();
    };
    lane("api", "shop", "claude@shop");
    lane("billing", "shop", "codex@shop");
    lane("docs", "shop", "codex@shop");
    lane("keys", "vault", "vaulter@vault");
    h.fetch_add(HOUR, std::sync::atomic::Ordering::SeqCst);

    // A blocked lane, a closed one with a PR, a cross-lane hold conflict.
    s.update_lane(
        LaneUpdate {
            agent: "manager".into(),
            lane: "billing".into(),
            status: Some(LaneStatus::Blocked),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    s.report_lane(
        LaneReportInput {
            agent: "codex@shop".into(),
            lane: "docs".into(),
            kind: ReportKind::Done,
            summary: "Docs rewritten".into(),
            progress: Some(100),
            status: None,
            links: vec![],
            artifacts: vec![],
            outcome: Some("New guide".into()),
            pr_url: Some("https://example.test/pr/9".into()),
        },
        None,
    )
    .unwrap();
    for (agent, lane) in [("claude@shop", "api"), ("codex@shop", "billing")] {
        s.acquire_holds(AcquireHolds {
            agent: agent.into(),
            project: "shop".into(),
            paths: vec!["src/shared.rs".into()],
            ttl_seconds: None,
            exclusive: true,
            reason: None,
            lane: Some(lane.into()),
        })
        .unwrap();
    }
    // A decision waiting, one answered.
    for q in ["Ship the API now?", "Rename billing tables?"] {
        s.ask_decision(
            NewDecision {
                agent: "claude@shop".into(),
                question: q.into(),
                options: opts(),
                lane: Some("api".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    }
    let first = s.decisions(DecisionFilter::default(), None).unwrap();
    s.answer_decision(
        AnswerDecision {
            decision: first.last().unwrap().decision_id.clone(),
            value: Some("ship".into()),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    // Spend over budget, and vault activity that a shop token must never see.
    s.create_spend_account(
        NewSpendAccount {
            name: "shop-api".into(),
            kind: SpendKind::ApiCredits,
            unit: "USD".into(),
            budget: Some(50.0),
            period: SpendPeriod::Monthly,
            reset_day: 1,
            cost_per_hour: None,
            requires_approval: false,
            approver: None,
            project: Some("shop".into()),
            description: None,
        },
        None,
    )
    .unwrap();
    s.record_spend(
        RecordSpend {
            agent: Some("claude@shop".into()),
            account: "shop-api".into(),
            amount: 60.0,
            ..Default::default()
        },
        None,
    )
    .unwrap();
    s.ask_decision(
        NewDecision {
            agent: "vaulter@vault".into(),
            question: "Rotate vault keys tonight?".into(),
            options: opts(),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    s.update_lane(
        LaneUpdate {
            agent: "manager".into(),
            lane: "keys".into(),
            status: Some(LaneStatus::Blocked),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    s
}

#[test]
fn summary_has_every_section() {
    let s = day();
    let sum = s.daily_summary(None, None).unwrap();
    assert_eq!(sum.lanes.open.get("blocked"), Some(&2));
    assert_eq!(sum.lanes.closed.len(), 1);
    assert_eq!(
        sum.lanes.closed[0].pr_url.as_deref(),
        Some("https://example.test/pr/9")
    );
    assert!(sum
        .lanes
        .stuck
        .iter()
        .any(|l| l.starts_with("billing (blocked")));
    assert!(sum.lanes.moved.iter().any(|m| m.contains("billing")));
    assert_eq!(sum.decisions.open.len(), 2);
    assert_eq!(sum.decisions.answered, 1);
    assert_eq!(sum.holds.conflicts, 1);
    assert_eq!(sum.holds.cross_lane_conflicts, 1);
    assert_eq!(sum.spend[0].account, "shop-api");
    assert_eq!(sum.spend[0].spent, 60.0);
    assert_eq!(sum.spend[0].status, BudgetStatus::Over);
    assert!(sum
        .attention
        .iter()
        .any(|a| a.starts_with("Decision for jacob from claude@shop")));
    assert!(sum
        .attention
        .iter()
        .any(|a| a == "shop-api is over budget (120%)"));
    assert!(sum
        .attention
        .iter()
        .any(|a| a.starts_with("Lane billing (blocked")));
    assert!(sum.agents_active.contains(&"claude@shop".to_string()));

    let md = &sum.markdown;
    assert!(
        md.starts_with("**Another: last 24h**\n**Needs you**\n"),
        "{md}"
    );
    assert!(
        md.contains("closed: docs (https://example.test/pr/9)"),
        "{md}"
    );
    assert!(md.contains("Decisions: 2 open, 1 answered, 0 timed out"));
    assert!(
        md.contains("Holds: 1 live, 1 conflict(s) (1 between lanes)"),
        "{md}"
    );
    assert!(
        md.contains("Requests: 0 open"),
        "decisions are not double-counted: {md}"
    );
    assert!(
        md.contains("Spend: shop-api 60 USD, 120% of budget"),
        "{md}"
    );
    assert!(md.lines().count() <= 16, "{md}");

    // A narrow window sees no older moves.
    let later = s
        .daily_summary(Some(Since::Time(s.now() + 1)), None)
        .unwrap();
    assert!(later.lanes.moved.is_empty());
    assert_eq!(
        later.decisions.open.len(),
        2,
        "open decisions are always shown"
    );
}

#[test]
fn summary_is_scoped() {
    let s = day();
    let t = s
        .create_token("manager", &["/work/shop".into()], None, None)
        .unwrap();
    let scope = s.verify_token(&t.secret).unwrap();
    let sum = s.daily_summary(None, Some(&scope)).unwrap();
    let all = serde_json::to_string(&sum).unwrap();
    for leak in ["vault", "keys", "Rotate", "vaulter"] {
        assert!(!all.contains(leak), "{leak} leaked: {all}");
    }
    assert_eq!(sum.decisions.open.len(), 1);
    assert_eq!(sum.lanes.open.get("blocked"), Some(&1));
    let full = s.daily_summary(None, None).unwrap();
    assert!(serde_json::to_string(&full)
        .unwrap()
        .contains("Rotate vault keys"));
}
