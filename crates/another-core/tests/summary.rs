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
        s.register_agent_trusted(RegisterAgent {
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

#[test]
fn since_event_ids_resolve_through_the_scoped_feed() {
    let s = day();
    let t = s
        .create_token("manager", &["/work/shop".into()], None, None)
        .unwrap();
    let scope = s.verify_token(&t.secret).unwrap();
    let events = s.events(500).unwrap();
    let lane_event = |name: &str| {
        events
            .iter()
            .find(|e| e.kind == "lane.created" && e.payload["name"] == name)
            .unwrap()
            .event_id
            .clone()
    };
    // A shop event works as a cursor for the shop token.
    s.daily_summary(Some(Since::After(lane_event("api"))), Some(&scope))
        .unwrap();
    // A vault event and a made-up id get the same not_found.
    let hidden = lane_event("keys");
    let e1 = s
        .daily_summary(Some(Since::After(hidden.clone())), Some(&scope))
        .unwrap_err();
    let e2 = s
        .daily_summary(Some(Since::After("no-such-event".into())), Some(&scope))
        .unwrap_err();
    assert_eq!((e1.code(), e2.code()), ("not_found", "not_found"));
    assert_eq!(
        e1.to_string().replace(&hidden, "X"),
        e2.to_string().replace("no-such-event", "X")
    );
    // Locally it resolves.
    s.daily_summary(Some(Since::After(hidden)), None).unwrap();
}

#[test]
fn people_hidden_budgets_and_truncation_in_the_digest() {
    let s = day();
    s.register_agent_trusted(RegisterAgent {
        name: "sam".into(),
        program: Program::Human,
        project: None,
        model: None,
        description: None,
    })
    .unwrap();
    s.heartbeat("sam", false).unwrap();
    // A zero budget: over, with no percentage to show.
    s.create_spend_account(
        NewSpendAccount {
            name: "domains".into(),
            kind: SpendKind::Domain,
            unit: "USD".into(),
            budget: Some(0.0),
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
            account: "domains".into(),
            amount: 12.0,
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let mut sum = s.daily_summary(None, None).unwrap();
    assert!(
        sum.people_active.contains(&"sam".to_string()),
        "{:?}",
        sum.people_active
    );
    assert!(!sum.agents_active.contains(&"sam".to_string()));
    assert!(
        !sum.agents_active.contains(&"another".to_string()),
        "the system agent isn't an agent at work"
    );
    assert!(
        sum.markdown.contains("People active: jacob, sam"),
        "{}",
        sum.markdown
    );
    assert!(
        sum.attention.iter().any(|a| a == "domains is over budget"),
        "{:?}",
        sum.attention
    );
    assert!(
        sum.markdown.contains("domains 12 USD, over budget"),
        "{}",
        sum.markdown
    );
    assert!(!sum.truncated);
    assert!(!sum.markdown.contains("Truncated"));
    sum.truncated = true;
    assert!(summary_markdown(&sum).contains("(Truncated: over 20000 events in a section"));
}

#[test]
fn scheduled_summary_by_mail_and_webhook() {
    use chrono::FixedOffset;
    // 2026-11-21 08:00 UTC.
    let (clock, h) = Clock::manual(NOV_21 + 8 * HOUR);
    let s = Store::open_in_memory(clock).unwrap();
    s.ensure_project("t", "/work/shop").unwrap();
    for (name, program) in [("jacob", Program::Human), ("manager", Program::GrokBot)] {
        s.register_agent_trusted(RegisterAgent {
            name: name.into(),
            program,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    }
    let utc = FixedOffset::east_opt(0).unwrap();
    let step = |ms: i64| h.fetch_add(ms, std::sync::atomic::Ordering::SeqCst);

    // Off by default: nothing is sent.
    assert!(s.summary_schedule().unwrap().daily_at.is_none());
    assert_eq!(s.send_scheduled_summary_at(utc).unwrap(), None);
    assert_eq!(
        s.set_summary_schedule(Some("25:00"), None)
            .unwrap_err()
            .code(),
        "invalid"
    );
    assert_eq!(
        s.set_summary_schedule(Some("09:00"), Some("sms:jacob"))
            .unwrap_err()
            .code(),
        "invalid"
    );

    // By mail, from the system agent, once a day after 09:00 local time.
    s.set_summary_schedule(Some("09:00"), Some("mail:jacob"))
        .unwrap();
    assert_eq!(s.send_scheduled_summary_at(utc).unwrap(), None, "08:00");
    // At UTC+2 it is already 10:00 there.
    let plus2 = FixedOffset::east_opt(2 * 3600).unwrap();
    assert_eq!(
        s.send_scheduled_summary_at(plus2).unwrap().as_deref(),
        Some("mail:jacob")
    );
    assert_eq!(
        s.send_scheduled_summary_at(plus2).unwrap(),
        None,
        "once a day"
    );
    let inbox = s
        .inbox(
            "jacob",
            InboxQuery {
                limit: 50,
                ..Default::default()
            },
        )
        .unwrap();
    let m = inbox
        .messages
        .iter()
        .find(|m| m.message.subject.starts_with("[summary]"))
        .expect("summary mail");
    assert_eq!(m.message.from, "another");
    assert!(
        m.message.body.starts_with("**Another: last 24h**"),
        "{}",
        m.message.body
    );
    assert!(s.summary_schedule().unwrap().last_sent_at.is_some());

    // By webhook: only local all-project webhooks with the daily_summary filter get it.
    s.set_summary_schedule(Some("09:00"), Some("webhook"))
        .unwrap();
    let local = s
        .create_webhook(
            NewWebhook {
                url: "https://example.test/hook".into(),
                filters: vec![WebhookFilter::DailySummary],
                project_globs: None,
                description: None,
            },
            None,
        )
        .unwrap();
    let everything = s
        .create_webhook(
            NewWebhook {
                url: "https://example.test/all".into(),
                filters: vec![WebhookFilter::All],
                project_globs: None,
                description: None,
            },
            None,
        )
        .unwrap();
    let t = s
        .create_token("manager", &["/work/shop".into()], None, None)
        .unwrap();
    let scope = s.verify_token(&t.secret).unwrap();
    let remote = s
        .create_webhook(
            NewWebhook {
                url: "https://example.test/bot".into(),
                filters: vec![WebhookFilter::DailySummary],
                project_globs: None,
                description: None,
            },
            Some(&scope),
        )
        .unwrap();
    s.queue_webhook_deliveries().unwrap();
    step(24 * HOUR);
    assert_eq!(
        s.send_scheduled_summary_at(utc).unwrap(),
        None,
        "08:00 UTC the next day"
    );
    assert_eq!(
        s.send_scheduled_summary_at(plus2).unwrap().as_deref(),
        Some("webhook")
    );
    s.queue_webhook_deliveries().unwrap();
    let got = |id: &str| {
        s.webhook_deliveries(Some(id), None, 50)
            .unwrap()
            .into_iter()
            .filter(|d| d.kind == "summary.daily")
            .collect::<Vec<_>>()
    };
    let d = got(&local.webhook.webhook_id);
    assert_eq!(d.len(), 1);
    let body = s
        .claim_due_deliveries(50)
        .unwrap()
        .into_iter()
        .find(|o| o.kind == "summary.daily")
        .unwrap()
        .body;
    assert!(
        body.contains("**Another: last"),
        "the rollup carries the digest: {body}"
    );
    assert_eq!(d[0].filter, "daily_summary");
    assert!(
        got(&everything.webhook.webhook_id).is_empty(),
        "`all` leaves it out"
    );
    assert!(
        got(&remote.webhook.webhook_id).is_empty(),
        "token webhooks never get it"
    );
    // Not in a token's feed either.
    let feed = s
        .activity_scoped(
            ActivityQuery {
                since: Some(Since::Time(0)),
                limit: 500,
                kinds: vec![],
            },
            Some(&scope),
        )
        .unwrap();
    assert!(feed.items.iter().all(|i| i.kind != "summary.daily"));
    // Off again.
    s.set_summary_schedule(None, None).unwrap();
    step(24 * HOUR);
    assert_eq!(s.send_scheduled_summary_at(plus2).unwrap(), None);
}

#[test]
fn sweepers_sharing_a_home_send_the_rollup_once() {
    let dir = tempfile::tempdir().unwrap();
    let (clock, _h) = Clock::manual(NOV_21 + 10 * HOUR);
    let first = Store::open_with_clock(dir.path(), clock.clone()).unwrap();
    first
        .register_agent_trusted(RegisterAgent {
            name: "jacob".into(),
            program: Program::Human,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    first
        .set_summary_schedule(Some("09:00"), Some("mail:jacob"))
        .unwrap();
    // Several servers on one home, all due at once.
    let utc = chrono::FixedOffset::east_opt(0).unwrap();
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let s = Store::open_with_clock(dir.path(), clock.clone()).unwrap();
            std::thread::spawn(move || s.send_scheduled_summary_at(utc).unwrap())
        })
        .collect();
    let sent = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(Option::is_some)
        .count();
    assert_eq!(sent, 1);
    let inbox = first
        .inbox(
            "jacob",
            InboxQuery {
                limit: 50,
                ..Default::default()
            },
        )
        .unwrap();
    let rollups = inbox
        .messages
        .iter()
        .filter(|m| m.message.subject.starts_with("[summary]"))
        .count();
    assert_eq!(rollups, 1);
}

#[test]
fn every_open_decision_counts_and_the_oldest_are_kept() {
    let (clock, h) = Clock::manual(NOV_21);
    let s = Store::open_in_memory(clock).unwrap();
    s.ensure_project("t", "/work/shop").unwrap();
    s.register_agent_trusted(RegisterAgent {
        name: "claude".into(),
        program: Program::Claude,
        project: Some("/work/shop".into()),
        model: None,
        description: None,
    })
    .unwrap();
    let mut first = String::new();
    for i in 0..(OPEN_DECISIONS_CAP + 1) {
        let d = s
            .ask_decision(
                NewDecision {
                    agent: "claude@shop".into(),
                    question: format!("Question {i}?"),
                    options: opts(),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        if i == 0 {
            first = d.decision_id;
        }
        h.fetch_add(1000, std::sync::atomic::Ordering::SeqCst);
    }
    let sum = s.daily_summary(None, None).unwrap();
    assert_eq!(sum.decisions.open_total as usize, OPEN_DECISIONS_CAP + 1);
    assert_eq!(sum.decisions.open.len(), OPEN_DECISIONS_CAP);
    assert_eq!(
        sum.decisions.open[0].decision_id, first,
        "the oldest is kept, first"
    );
    assert!(sum.truncated);
    assert!(sum
        .markdown
        .contains(&format!("Decisions: {} open", OPEN_DECISIONS_CAP + 1)));
}
