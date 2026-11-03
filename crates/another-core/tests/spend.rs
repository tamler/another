//! Spend ledger: accounts (local only), entries, periods and reset days, burn-down figures,
//! threshold events, hourly leases (expiry, renewal), the approval gate through decisions,
//! token scoping (project and shared accounts), and upgrade from v9.

use another_core::*;

type Handle = std::sync::Arc<std::sync::atomic::AtomicI64>;

// 2026-11-21T00:00:00Z: day 21 of a period that started on the 1st.
const NOV_21: i64 = 1_795_219_200_000;
const HOUR: i64 = 3_600_000;

fn store() -> (Store, Handle) {
    let (clock, h) = Clock::manual(NOV_21);
    let s = Store::open_in_memory(clock).unwrap();
    s.ensure_project("t", "/work/shop").unwrap();
    s.ensure_project("t", "/work/vault").unwrap();
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
    s.create_lane(
        NewLane {
            agent: "manager".into(),
            name: "train".into(),
            project: "shop".into(),
            lead: "codex@shop".into(),
            charter: String::new(),
            status: Some(LaneStatus::Working),
        },
        None,
    )
    .unwrap();
    (s, h)
}

fn advance(h: &Handle, ms: i64) {
    h.fetch_add(ms, std::sync::atomic::Ordering::SeqCst);
}

fn account(
    s: &Store,
    name: &str,
    kind: SpendKind,
    unit: &str,
    budget: Option<f64>,
    f: impl FnOnce(&mut NewSpendAccount),
) -> SpendAccount {
    let mut a = NewSpendAccount {
        name: name.into(),
        kind,
        unit: unit.into(),
        budget,
        period: SpendPeriod::Monthly,
        reset_day: 1,
        cost_per_hour: None,
        requires_approval: false,
        approver: None,
        project: None,
        description: None,
    };
    f(&mut a);
    s.create_spend_account(a, None).unwrap()
}

fn record(s: &Store, agent: &str, account: &str, amount: f64, lane: Option<&str>) -> SpendEntry {
    s.record_spend(
        RecordSpend {
            agent: Some(agent.into()),
            account: account.into(),
            amount,
            lane: lane.map(Into::into),
            ..Default::default()
        },
        None,
    )
    .unwrap()
}

fn report(s: &Store, scope: Option<&TokenScope>) -> SpendReport {
    s.spend_report(SpendQuery::default(), scope).unwrap()
}

fn find<'a>(r: &'a SpendReport, name: &str) -> &'a AccountSpend {
    r.accounts.iter().find(|a| a.account.name == name).unwrap()
}

#[test]
fn burn_down_by_period_lane_and_agent() {
    let (s, _) = store();
    account(
        &s,
        "claude-api",
        SpendKind::ApiCredits,
        "USD",
        Some(200.0),
        |_| {},
    );
    account(
        &s,
        "codex-resets",
        SpendKind::PlanReset,
        "resets",
        Some(2.0),
        |a| a.reset_day = 15,
    );
    // Name rules and uniqueness.
    let dup = s.create_spend_account(
        NewSpendAccount {
            name: "Claude-API".into(),
            kind: SpendKind::ApiCredits,
            unit: "USD".into(),
            budget: None,
            period: SpendPeriod::Monthly,
            reset_day: 1,
            cost_per_hour: None,
            requires_approval: false,
            approver: None,
            project: None,
            description: None,
        },
        None,
    );
    assert_eq!(dup.unwrap_err().code(), "conflict");

    // Last month's spend is not in this period.
    s.record_spend(
        RecordSpend {
            agent: Some("claude@shop".into()),
            account: "claude-api".into(),
            amount: 50.0,
            at: Some(NOV_21 - 30 * 24 * HOUR),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    record(&s, "claude@shop", "claude-api", 100.0, None);
    record(&s, "codex@shop", "claude-api", 42.1, Some("train"));
    record(&s, "codex@shop", "codex-resets", 1.0, Some("train"));

    let r = report(&s, None);
    let api = find(&r, "claude-api");
    assert_eq!(api.spent, 142.1);
    assert_eq!(api.remaining, Some(57.9));
    assert_eq!(api.used_pct, Some(71.05));
    assert_eq!(api.days_total, Some(30.0));
    assert_eq!(api.days_elapsed, Some(20.0));
    assert_eq!(api.projected, Some(213.15));
    assert_eq!(api.status, BudgetStatus::Warning, "on pace to exceed");
    assert_eq!(
        api.period_start.as_deref(),
        Some("2026-11-01T00:00:00.000Z")
    );
    let resets = find(&r, "codex-resets");
    assert_eq!(
        resets.period_start.as_deref(),
        Some("2026-11-15T00:00:00.000Z")
    );
    assert_eq!(resets.used_pct, Some(50.0));
    assert!(r
        .by_lane
        .iter()
        .any(|g| g.key == "train" && g.account == "claude-api" && g.amount == 42.1));
    assert!(r
        .by_agent
        .iter()
        .any(|g| g.key == "claude@shop" && g.amount == 100.0));
    assert!(
        r.markdown.contains(
            "`claude-api` 142.10 / 200 USD  71%  ███████░░░  day 20/30 → 213.15 (over pace)"
        ),
        "{}",
        r.markdown
    );
    assert!(r.markdown.lines().count() <= 6);

    let prev = s
        .spend_report(
            SpendQuery {
                period: PeriodSel::Previous,
                ..Default::default()
            },
            None,
        )
        .unwrap();
    assert_eq!(find(&prev, "claude-api").spent, 50.0);

    // Threshold events: 80% once, then 100%.
    record(&s, "claude@shop", "claude-api", 20.0, None);
    record(&s, "claude@shop", "claude-api", 1.0, None);
    record(&s, "claude@shop", "claude-api", 40.0, None);
    let pcts: Vec<f64> = s
        .events(500)
        .unwrap()
        .iter()
        .filter(|e| e.kind == "spend.budget_threshold")
        .map(|e| e.payload["pct"].as_f64().unwrap())
        .collect();
    assert_eq!(pcts, vec![80.0, 100.0]);
    assert_eq!(
        find(&report(&s, None), "claude-api").status,
        BudgetStatus::Over
    );

    // Validation.
    let neg = s.record_spend(
        RecordSpend {
            account: "claude-api".into(),
            amount: -1.0,
            ..Default::default()
        },
        None,
    );
    assert_eq!(neg.unwrap_err().code(), "invalid");
}

#[test]
fn leases_accrue_end_expire_and_renew() {
    let (s, clock) = store();
    account(&s, "gpu-a100", SpendKind::GpuPod, "USD", Some(100.0), |a| {
        a.cost_per_hour = Some(2.1);
        a.project = Some("shop".into());
    });
    let out = s
        .borrow_spend(
            BorrowSpend {
                agent: "codex@shop".into(),
                account: "gpu-a100".into(),
                lane: Some("train".into()),
                ttl_seconds: Some(3 * 3600),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    assert_eq!(out.lease.status, SpendLeaseStatus::Active);
    assert!(out.decision_id.is_none());
    advance(&clock, 2 * HOUR);
    let r = report(&s, None);
    let gpu = find(&r, "gpu-a100");
    assert_eq!((gpu.spent, gpu.accrued, gpu.active_leases), (0.0, 4.2, 1));
    // Only the owner can end it.
    assert_eq!(
        s.release_spend("claude@shop", &out.lease.lease_id, None)
            .unwrap_err()
            .code(),
        "forbidden"
    );
    let ended = s
        .release_spend("codex@shop", &out.lease.lease_id, None)
        .unwrap();
    assert_eq!(ended.status, SpendLeaseStatus::Ended);
    assert_eq!(ended.cost, 4.2);
    assert_eq!(find(&report(&s, None), "gpu-a100").spent, 4.2);

    // A lease that isn't renewed ends at its expiry; heartbeats renew it.
    let l = s
        .borrow_spend(
            BorrowSpend {
                agent: "codex@shop".into(),
                account: "gpu-a100".into(),
                ttl_seconds: Some(3600),
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .lease;
    advance(&clock, HOUR / 2);
    s.heartbeat("codex@shop", false).unwrap();
    advance(&clock, 50 * 60_000);
    s.sweep().unwrap();
    assert_eq!(s.spend_leases(false, None).unwrap().len(), 1, "renewed");
    advance(&clock, HOUR);
    s.sweep().unwrap();
    let all = s.spend_leases(true, None).unwrap();
    let done = all.iter().find(|x| x.lease_id == l.lease_id).unwrap();
    assert_eq!(done.status, SpendLeaseStatus::Ended);
    assert_eq!(done.cost, 3.15, "1.5 h until the renewed expiry");
    let kinds: Vec<String> = s.events(500).unwrap().into_iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&"spend.lease_started".to_string()));
    assert!(kinds.contains(&"spend.lease_ended".to_string()));

    // No rate: borrowing needs one.
    account(&s, "vps", SpendKind::Server, "USD", None, |_| {});
    let r = s.borrow_spend(
        BorrowSpend {
            agent: "codex@shop".into(),
            account: "vps".into(),
            ..Default::default()
        },
        None,
    );
    assert_eq!(r.unwrap_err().code(), "invalid");
}

#[test]
fn approval_gate_creates_a_decision() {
    let (s, clock) = store();
    account(&s, "gpu-h100", SpendKind::GpuPod, "USD", Some(500.0), |a| {
        a.cost_per_hour = Some(4.0);
        a.requires_approval = true;
        a.approver = Some("sam".into());
    });
    let ask = |lane: Option<&str>| {
        s.borrow_spend(
            BorrowSpend {
                agent: "codex@shop".into(),
                account: "gpu-h100".into(),
                lane: lane.map(Into::into),
                hours: Some(3.0),
                note: Some("fine-tune run".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap()
    };
    let pending = ask(Some("train"));
    assert_eq!(pending.lease.status, SpendLeaseStatus::PendingApproval);
    let decision_id = pending.decision_id.clone().unwrap();
    let d = s.decision(&decision_id, None).unwrap();
    assert_eq!(d.human, "sam");
    assert!(
        d.question
            .contains("borrowing gpu-h100 at 4 USD/h for 3 h (about 12 USD)"),
        "{}",
        d.question
    );
    assert_eq!(
        d.options
            .iter()
            .map(|o| o.value.as_str())
            .collect::<Vec<_>>(),
        vec!["approve", "deny"]
    );
    assert_eq!(
        s.lane("train", None).unwrap().lane.status,
        LaneStatus::NeedsDecision
    );
    assert_eq!(
        find(&report(&s, None), "gpu-h100").active_leases,
        0,
        "not started yet"
    );

    advance(&clock, HOUR);
    s.answer_decision(
        AnswerDecision {
            decision: decision_id,
            value: Some("approve".into()),
            relayed_by: Some("manager".into()),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let leases = s.spend_leases(false, None).unwrap();
    let l = leases
        .iter()
        .find(|l| l.lease_id == pending.lease.lease_id)
        .unwrap();
    assert_eq!(l.status, SpendLeaseStatus::Active);
    assert_eq!(l.cost, 0.0, "the clock starts at approval");
    assert_eq!(
        s.lane("train", None).unwrap().lane.status,
        LaneStatus::Working
    );

    // Deny, and withdraw (cancelling the decision) both mark the lease denied.
    let denied = ask(Some("train"));
    s.answer_decision(
        AnswerDecision {
            decision: denied.decision_id.clone().unwrap(),
            value: Some("deny".into()),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let withdrawn = ask(None);
    s.cancel_decision(
        "codex@shop",
        withdrawn.decision_id.as_ref().unwrap(),
        None,
        None,
    )
    .unwrap();
    let all = s.spend_leases(true, None).unwrap();
    for id in [&denied.lease.lease_id, &withdrawn.lease.lease_id] {
        assert_eq!(
            all.iter().find(|l| &l.lease_id == id).unwrap().status,
            SpendLeaseStatus::Denied
        );
    }
    let mut ds = s
        .decisions(
            DecisionFilter {
                all: true,
                ..Default::default()
            },
            None,
        )
        .unwrap();
    ds.retain(|d| d.human == "sam");
    assert_eq!(ds.len(), 3);
    assert!(s
        .events(500)
        .unwrap()
        .iter()
        .any(|e| e.kind == "spend.approval_requested"));
}

#[test]
fn token_scope_hides_other_projects_spend() {
    let (s, _) = store();
    account(&s, "shop-gpu", SpendKind::GpuPod, "USD", Some(100.0), |a| {
        a.project = Some("shop".into())
    });
    account(
        &s,
        "vault-domain",
        SpendKind::Domain,
        "USD",
        Some(20.0),
        |a| a.project = Some("vault".into()),
    );
    account(
        &s,
        "claude-api",
        SpendKind::ApiCredits,
        "USD",
        Some(200.0),
        |_| {},
    );
    record(&s, "claude@shop", "shop-gpu", 10.0, None);
    record(&s, "vaulter@vault", "vault-domain", 12.0, None);
    record(&s, "claude@shop", "claude-api", 30.0, None);
    record(&s, "vaulter@vault", "claude-api", 70.0, None);

    let t = s
        .create_token("manager", &["/work/shop".into()], None, None)
        .unwrap();
    let shop = s.verify_token(&t.secret).unwrap();

    let names: Vec<String> = s
        .spend_accounts(Some(&shop))
        .unwrap()
        .into_iter()
        .map(|a| a.name)
        .collect();
    assert_eq!(names, vec!["claude-api", "shop-gpu"]);
    let r = report(&s, Some(&shop));
    assert!(r.accounts.iter().all(|a| a.account.name != "vault-domain"));
    let shared = find(&r, "claude-api");
    assert_eq!(
        shared.spent, 30.0,
        "only shop's share of the shared account"
    );
    assert!(shared.partial);
    assert_eq!(
        shared.budget, None,
        "remaining would reveal other projects' spend"
    );
    assert_eq!(shared.remaining, None);
    assert_eq!(shared.projected, None);
    assert!(!r.markdown.contains("vault"));
    assert!(!serde_json::to_string(&r).unwrap().contains("vaulter"));
    assert_eq!(find(&r, "shop-gpu").spent, 10.0);
    // Local view has everything.
    assert_eq!(find(&report(&s, None), "claude-api").spent, 100.0);

    // Remote writes: accounts are local-only; spend must be attributed to the token's projects.
    let create = s.create_spend_account(
        NewSpendAccount {
            name: "sneak".into(),
            kind: SpendKind::Server,
            unit: "USD".into(),
            budget: None,
            period: SpendPeriod::Monthly,
            reset_day: 1,
            cost_per_hour: None,
            requires_approval: false,
            approver: None,
            project: None,
            description: None,
        },
        Some(&shop),
    );
    assert_eq!(create.unwrap_err().code(), "forbidden");
    let r = s.record_spend(
        RecordSpend {
            agent: Some("manager".into()),
            account: "vault-domain".into(),
            amount: 1.0,
            ..Default::default()
        },
        Some(&shop),
    );
    assert_eq!(r.unwrap_err().code(), "forbidden");
    let r = s.record_spend(
        RecordSpend {
            agent: Some("manager".into()),
            account: "claude-api".into(),
            amount: 1.0,
            project: Some("vault".into()),
            ..Default::default()
        },
        Some(&shop),
    );
    assert_eq!(r.unwrap_err().code(), "forbidden");
    s.record_spend(
        RecordSpend {
            agent: Some("manager".into()),
            account: "claude-api".into(),
            amount: 1.0,
            lane: Some("train".into()),
            ..Default::default()
        },
        Some(&shop),
    )
    .unwrap();

    // Spend events from vault never reach the shop-scoped feed.
    let feed = s
        .activity_scoped(
            ActivityQuery {
                since: Some(Since::Time(0)),
                limit: 500,
                kinds: vec![ActivityKind::Spend],
            },
            Some(&shop),
        )
        .unwrap();
    assert!(!feed.items.is_empty());
    let text: String = feed.items.iter().map(|i| i.summary.clone()).collect();
    assert!(!text.contains("vault") && !text.contains("70"), "{text}");
}

#[test]
fn upgrades_a_database_created_before_spend() {
    let dir = tempfile::tempdir().unwrap();
    {
        let old = Store::open_at_schema(dir.path(), 9).unwrap();
        old.ensure_project("t", "/work/shop").unwrap();
    }
    let s = Store::open(dir.path()).unwrap();
    assert!(s.spend_accounts(None).unwrap().is_empty());
    account(&s, "x", SpendKind::Server, "USD", Some(5.0), |_| {});
    assert!(Store::schema_version() >= 10);
}
