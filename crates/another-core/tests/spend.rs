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
        s.register_agent_trusted(RegisterAgent {
            name: name.into(),
            program: if name == "manager" {
                Program::GrokBot
            } else {
                Program::Claude
            },
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
    // Budget figures are for the whole shared account (read-only), not who used it.
    assert_eq!(shared.budget, Some(200.0));
    assert_eq!(shared.remaining, Some(100.0));
    assert_eq!(shared.used_pct, Some(50.0));
    assert!(r.markdown.contains("100 of 200 USD left"), "{}", r.markdown);
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
    // Another project's account is the same not_found as a missing one.
    let rec = |account: &str| {
        s.record_spend(
            RecordSpend {
                agent: Some("manager".into()),
                account: account.into(),
                amount: 1.0,
                ..Default::default()
            },
            Some(&shop),
        )
        .unwrap_err()
    };
    let (hidden, missing) = (rec("vault-domain"), rec("no-such-account"));
    assert_eq!((hidden.code(), missing.code()), ("not_found", "not_found"));
    assert_eq!(
        hidden.to_string().replace("vault-domain", "X"),
        missing.to_string().replace("no-such-account", "X")
    );
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
    assert_eq!(
        r.unwrap_err().code(),
        "not_found",
        "a hidden project is a missing one"
    );
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

fn gpu_with_approval(s: &Store) {
    account(s, "gpu-h100", SpendKind::GpuPod, "USD", Some(500.0), |a| {
        a.cost_per_hour = Some(4.0);
        a.requires_approval = true;
        a.approver = Some("sam".into());
    });
}

fn borrow(s: &Store, agent: &str, account: &str, lane: Option<&str>) -> SpendLeaseOutcome {
    s.borrow_spend(
        BorrowSpend {
            agent: agent.into(),
            account: account.into(),
            lane: lane.map(Into::into),
            ..Default::default()
        },
        None,
    )
    .unwrap()
}

#[test]
fn releasing_a_pending_lease_withdraws_its_decision() {
    let (s, _) = store();
    gpu_with_approval(&s);
    let pending = borrow(&s, "codex@shop", "gpu-h100", Some("train"));
    let decision_id = pending.decision_id.clone().unwrap();
    assert_eq!(
        s.lane("train", None).unwrap().lane.status,
        LaneStatus::NeedsDecision
    );

    let l = s
        .release_spend("codex@shop", &pending.lease.lease_id, None)
        .unwrap();
    assert_eq!(l.status, SpendLeaseStatus::Denied);
    assert_eq!(
        s.decision(&decision_id, None).unwrap().status,
        DecisionStatus::Cancelled,
        "the approver is no longer asked"
    );
    assert_eq!(
        s.lane("train", None).unwrap().lane.status,
        LaneStatus::Working,
        "the lane leaves needs_decision"
    );
    let ev = s.events(200).unwrap();
    let w = ev
        .iter()
        .find(|e| e.kind == "spend.lease_withdrawn")
        .expect("spend.lease_withdrawn");
    assert_eq!(w.payload["lease_id"], pending.lease.lease_id);
    assert_eq!(w.payload["decision_id"], decision_id);
    // Answering it now is refused: it is closed.
    let late = s.answer_decision(
        AnswerDecision {
            decision: decision_id,
            value: Some("approve".into()),
            relayed_by: Some("manager".into()),
            ..Default::default()
        },
        None,
    );
    assert_eq!(late.unwrap_err().code(), "conflict");
}

#[test]
fn the_borrower_cannot_approve_its_own_spend() {
    let (s, _) = store();
    gpu_with_approval(&s);
    // A manager bot may relay answers, but not for a lease it asked for itself.
    let pending = borrow(&s, "manager", "gpu-h100", Some("train"));
    let answer = |by: &str| {
        s.answer_decision(
            AnswerDecision {
                decision: pending.decision_id.clone().unwrap(),
                value: Some("approve".into()),
                relayed_by: Some(by.into()),
                ..Default::default()
            },
            None,
        )
    };
    assert_eq!(answer("manager").unwrap_err().code(), "forbidden");
    assert_eq!(
        answer("codex@shop").unwrap_err().code(),
        "forbidden",
        "not a manager or person"
    );
    let leases = s.spend_leases(false, None).unwrap();
    assert_eq!(leases[0].status, SpendLeaseStatus::PendingApproval);
    // The person (or another manager) can.
    s.register_agent_trusted(RegisterAgent {
        name: "bot2".into(),
        program: Program::GrokBot,
        project: None,
        model: None,
        description: None,
    })
    .unwrap();
    answer("bot2").unwrap();
    assert_eq!(
        s.spend_leases(false, None).unwrap()[0].status,
        SpendLeaseStatus::Active
    );
}

fn gpu_resource(s: &Store, ttl: i64) -> Resource {
    s.register_resource(RegisterResource {
        agent: "claude@shop".into(),
        kind: ResourceKind::Gpu,
        name: "pod:a100".into(),
        endpoint: None,
        port: None,
        pid: None,
        project: Some("shop".into()),
        purpose: Some("rented pod".into()),
        mode: LeaseMode::Shared,
        ttl_seconds: Some(ttl),
        metadata: None,
    })
    .unwrap()
    .resource
}

#[test]
fn borrowing_a_linked_resource_goes_through_the_gate_and_accrues() {
    let (s, clock) = store();
    gpu_with_approval(&s);
    let res = gpu_resource(&s, 86_400);
    let target = ResourceRef::parse("gpu:pod:a100").unwrap();
    // Linking is local-only and needs an hourly rate.
    let t = s
        .create_token("manager", &["/work/shop".into()], None, None)
        .unwrap();
    let shop = s.verify_token(&t.secret).unwrap();
    assert_eq!(
        s.link_resource_spend(&target, Some("gpu-h100"), Some(&shop))
            .unwrap_err()
            .code(),
        "forbidden"
    );
    account(&s, "flat", SpendKind::Server, "USD", None, |_| {});
    assert_eq!(
        s.link_resource_spend(&target, Some("flat"), None)
            .unwrap_err()
            .code(),
        "invalid"
    );
    let linked = s
        .link_resource_spend(&target, Some("gpu-h100"), None)
        .unwrap();
    assert_eq!(linked.spend_account.as_deref(), Some("gpu-h100"));

    // Borrowing waits for approval: no resource lease yet.
    let out = s
        .borrow_resource("codex@shop", &target, Some(10800))
        .unwrap();
    assert_eq!(out.outcome, "pending_approval");
    assert!(out.lease.is_none());
    let decision_id = out.decision_id.clone().unwrap();
    assert_eq!(out.resource.refcount, 1, "only the owner");
    // Asking again doesn't open a second request.
    let again = s
        .borrow_resource("codex@shop", &target, Some(10800))
        .unwrap();
    assert_eq!(again.decision_id.as_deref(), Some(decision_id.as_str()));
    assert_eq!(s.spend_leases(false, None).unwrap().len(), 1);

    // Approval starts the spend lease and takes the resource lease.
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
    let r = s.resource(&target).unwrap();
    assert!(r
        .leases
        .iter()
        .any(|l| l.agent == "codex@shop" && l.role == "borrower"));
    let lease = &s.spend_leases(false, None).unwrap()[0];
    assert_eq!(lease.status, SpendLeaseStatus::Active);
    assert_eq!(lease.resource_id.as_deref(), Some(res.resource_id.as_str()));

    // Two hours later the borrower releases the resource: the spend lease ends with it.
    advance(&clock, 2 * HOUR);
    s.borrow_resource("codex@shop", &target, Some(10800))
        .unwrap(); // renew
    s.release_resource("codex@shop", &target, false).unwrap();
    assert!(s.spend_leases(false, None).unwrap().is_empty());
    let ended = &s.spend_leases(true, None).unwrap()[0];
    assert_eq!(ended.status, SpendLeaseStatus::Ended);
    assert_eq!(ended.cost, 8.0, "2 h at 4 USD/h");
    assert_eq!(find(&report(&s, None), "gpu-h100").spent, 8.0);

    // Borrowing again with an active approval-free account: starts at once, and an expired
    // resource lease ends the spend lease at the lease's expiry.
    s.update_spend_account(
        "gpu-h100",
        SpendAccountUpdate {
            requires_approval: Some(false),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let out = s
        .borrow_resource("codex@shop", &target, Some(3600))
        .unwrap();
    assert_eq!(out.outcome, "borrowed");
    advance(&clock, 3 * HOUR);
    s.sweep().unwrap();
    let all = s.spend_leases(true, None).unwrap();
    let last = all
        .iter()
        .find(|l| l.status == SpendLeaseStatus::Ended && l.cost == 4.0);
    assert!(last.is_some(), "charged up to the lease expiry: {all:?}");

    // Unlinked resources borrow as before.
    s.link_resource_spend(&target, None, None).unwrap();
    let out = s.borrow_resource("codex@shop", &target, None).unwrap();
    assert_eq!(out.outcome, "borrowed");
    assert!(s.spend_leases(false, None).unwrap().is_empty());
}

#[test]
fn ending_a_session_closes_spend_leases() {
    let (s, clock) = store();
    gpu_with_approval(&s);
    account(&s, "cpu", SpendKind::Server, "USD", None, |a| {
        a.cost_per_hour = Some(1.0)
    });
    let active = borrow(&s, "codex@shop", "cpu", None);
    let pending = borrow(&s, "codex@shop", "gpu-h100", Some("train"));
    advance(&clock, HOUR);
    s.session("codex@shop", SessionPhase::End, SessionInfo::default())
        .unwrap();
    let all = s.spend_leases(true, None).unwrap();
    let get = |id: &str| all.iter().find(|l| l.lease_id == id).unwrap();
    assert_eq!(get(&active.lease.lease_id).status, SpendLeaseStatus::Ended);
    assert_eq!(get(&active.lease.lease_id).cost, 1.0);
    assert_eq!(
        get(&pending.lease.lease_id).status,
        SpendLeaseStatus::Denied
    );
    assert_eq!(
        s.decision(pending.decision_id.as_ref().unwrap(), None)
            .unwrap()
            .status,
        DecisionStatus::Cancelled
    );
    assert_eq!(
        s.lane("train", None).unwrap().lane.status,
        LaneStatus::Working
    );
}

#[test]
fn round3_linked_release_explicit_month_and_period_checks() {
    let (s, clock) = store();
    account(&s, "pods", SpendKind::GpuPod, "USD", Some(100.0), |a| {
        a.cost_per_hour = Some(2.0)
    });
    gpu_resource(&s, 86_400);
    let target = ResourceRef::parse("gpu:pod:a100").unwrap();
    s.link_resource_spend(&target, Some("pods"), None).unwrap();
    s.borrow_resource("codex@shop", &target, Some(7200))
        .unwrap();
    let lease = s.spend_leases(false, None).unwrap()[0].clone();
    // The spend lease can't be ended while its resource lease is held.
    let e = s
        .release_spend("codex@shop", &lease.lease_id, None)
        .unwrap_err();
    assert_eq!(e.code(), "conflict");
    assert!(e.to_string().contains("release the resource"), "{e}");

    // An explicit month naming the current period counts open leases like `current`.
    advance(&clock, HOUR);
    let cur = find(&report(&s, None), "pods").accrued;
    assert_eq!(cur, 2.0);
    let named = s
        .spend_report(
            SpendQuery {
                period: "2026-11".parse().unwrap(),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    assert_eq!(find(&named, "pods").accrued, 2.0);
    assert_eq!(find(&named, "pods").active_leases, 1);

    // Out-of-range years are invalid input, never a panic.
    for bad in ["999999-01", "0-05", "10000-01"] {
        assert_eq!(
            bad.parse::<PeriodSel>().unwrap_err().code(),
            "invalid",
            "{bad}"
        );
    }

    // Releasing the resource ends the spend lease with it.
    s.release_resource("codex@shop", &target, false).unwrap();
    assert!(s.spend_leases(false, None).unwrap().is_empty());

    // A hidden lease is the same not_found as a missing one.
    account(&s, "vault-cpu", SpendKind::Server, "USD", None, |a| {
        a.cost_per_hour = Some(1.0);
        a.project = Some("vault".into());
    });
    let vl = borrow(&s, "vaulter@vault", "vault-cpu", None);
    let t = s
        .create_token("codex@shop", &["/work/shop".into()], None, None)
        .unwrap();
    let shop = s.verify_token(&t.secret).unwrap();
    let e = s
        .release_spend("codex@shop", &vl.lease.lease_id, Some(&shop))
        .unwrap_err();
    assert_eq!(e.code(), "not_found");
}

#[test]
fn round4_reset_split_kept_resources_and_scoped_projects() {
    let (s, clock) = store();
    // A lease from Nov 30 23:00 to Dec 1 01:00 is charged one hour to each month.
    account(&s, "cpu", SpendKind::Server, "USD", Some(100.0), |a| {
        a.cost_per_hour = Some(3.0)
    });
    advance(&clock, 9 * 24 * HOUR + 23 * HOUR);
    let l = s
        .borrow_spend(
            BorrowSpend {
                agent: "codex@shop".into(),
                account: "cpu".into(),
                ttl_seconds: Some(4 * 3600),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    advance(&clock, 2 * HOUR);
    s.release_spend("codex@shop", &l.lease.lease_id, None)
        .unwrap();
    let month = |m: &str| {
        let r = s
            .spend_report(
                SpendQuery {
                    period: m.parse().unwrap(),
                    account: Some("cpu".into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        find(&r, "cpu").spent
    };
    assert_eq!((month("2026-11"), month("2026-12")), (3.0, 3.0));

    // --keep-resources keeps the linked spend lease charging with the resource lease.
    account(&s, "pods", SpendKind::GpuPod, "USD", None, |a| {
        a.cost_per_hour = Some(2.0)
    });
    gpu_resource(&s, 86_400);
    let target = ResourceRef::parse("gpu:pod:a100").unwrap();
    s.link_resource_spend(&target, Some("pods"), None).unwrap();
    s.borrow_resource("codex@shop", &target, Some(3600))
        .unwrap();
    advance(&clock, 30 * 60_000);
    s.session(
        "codex@shop",
        SessionPhase::End,
        SessionInfo {
            keep_resources: true,
            ..Default::default()
        },
    )
    .unwrap();
    advance(&clock, 45 * 60_000);
    s.sweep().unwrap();
    let open = s.spend_leases(false, None).unwrap();
    assert!(
        open.iter()
            .any(|l| l.account == "pods" && l.status == SpendLeaseStatus::Active),
        "renewed with the kept resource lease: {open:?}"
    );

    // An explicit project in spend_record is looked up in the token's scope.
    account(&s, "shared", SpendKind::ApiCredits, "USD", None, |_| {});
    let t = s
        .create_token("codex@shop", &["/work/shop".into()], None, None)
        .unwrap();
    let shop = s.verify_token(&t.secret).unwrap();
    let rec = |project: &str| {
        s.record_spend(
            RecordSpend {
                agent: Some("codex@shop".into()),
                account: "shared".into(),
                amount: 1.0,
                project: Some(project.into()),
                ..Default::default()
            },
            Some(&shop),
        )
        .unwrap_err()
    };
    let (hidden, missing) = (rec("vault"), rec("nowhere"));
    assert_eq!((hidden.code(), missing.code()), ("not_found", "not_found"));
    assert_eq!(
        hidden.to_string().replace("vault", "X"),
        missing.to_string().replace("nowhere", "X")
    );
}

#[test]
fn report_agent_filters_resolve_in_scope() {
    let (s, _) = store();
    for p in ["/work/shop", "/work/vault"] {
        s.register_agent_trusted(RegisterAgent {
            name: "dup".into(),
            program: Program::Claude,
            project: Some(p.into()),
            model: None,
            description: None,
        })
        .unwrap();
    }
    account(&s, "shared", SpendKind::ApiCredits, "USD", None, |_| {});
    record(&s, "dup@shop", "shared", 2.0, None);
    record(&s, "dup@vault", "shared", 5.0, None);
    let t = s
        .create_token("manager", &["/work/shop".into()], None, None)
        .unwrap();
    let shop = s.verify_token(&t.secret).unwrap();
    let by = |agent: &str| {
        s.spend_report(
            SpendQuery {
                agent: Some(agent.into()),
                ..Default::default()
            },
            Some(&shop),
        )
    };
    // The bare name means the visible dup@shop, without an ambiguity naming dup@vault.
    assert_eq!(find(&by("dup").unwrap(), "shared").spent, 2.0);
    // A hidden agent and a missing one fail the same way.
    let (hidden, missing) = (by("dup@vault").unwrap_err(), by("ghost@vault").unwrap_err());
    assert_eq!((hidden.code(), missing.code()), ("not_found", "not_found"));
    assert_eq!(
        hidden.to_string().replace("dup@vault", "X"),
        missing.to_string().replace("ghost@vault", "X")
    );
}

#[test]
fn a_lease_ending_on_the_reset_is_charged_to_the_period_it_ran_in() {
    let (s, clock) = store();
    account(&s, "cpu", SpendKind::Server, "USD", Some(100.0), |a| {
        a.cost_per_hour = Some(3.0)
    });
    advance(&clock, 9 * 24 * HOUR + 23 * HOUR); // Nov 30 23:00
    let l = s
        .borrow_spend(
            BorrowSpend {
                agent: "codex@shop".into(),
                account: "cpu".into(),
                ttl_seconds: Some(4 * 3600),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    advance(&clock, HOUR); // Dec 1 00:00, exactly the reset
    s.release_spend("codex@shop", &l.lease.lease_id, None)
        .unwrap();
    let month = |m: &str| {
        let r = s
            .spend_report(
                SpendQuery {
                    period: m.parse().unwrap(),
                    account: Some("cpu".into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        find(&r, "cpu").spent
    };
    assert_eq!((month("2026-11"), month("2026-12")), (3.0, 0.0));
}

#[test]
fn a_gated_re_register_refuses_changes_it_could_not_apply() {
    let (s, _) = store();
    gpu_with_approval(&s);
    gpu_resource(&s, 86_400);
    let target = ResourceRef::parse("gpu:pod:a100").unwrap();
    s.link_resource_spend(&target, Some("gpu-h100"), None)
        .unwrap();
    s.release_resource("claude@shop", &target, false).unwrap();
    let e = s
        .register_resource(RegisterResource {
            agent: "codex@shop".into(),
            kind: ResourceKind::Gpu,
            name: "pod:a100".into(),
            endpoint: Some("https://new.example.test".into()),
            port: None,
            pid: None,
            project: Some("shop".into()),
            purpose: None,
            mode: LeaseMode::Shared,
            ttl_seconds: Some(3600),
            metadata: None,
        })
        .unwrap_err();
    assert_eq!(e.code(), "conflict", "{e}");
    assert!(e.to_string().contains("approval"), "{e}");
    // A different project is a change too.
    let e = s
        .register_resource(RegisterResource {
            agent: "codex@shop".into(),
            kind: ResourceKind::Gpu,
            name: "pod:a100".into(),
            endpoint: None,
            port: None,
            pid: None,
            project: Some("vault".into()),
            purpose: None,
            mode: LeaseMode::Shared,
            ttl_seconds: Some(3600),
            metadata: None,
        })
        .unwrap_err();
    assert_eq!(e.code(), "conflict", "{e}");
    // Nothing was left waiting for an approval that would drop the change.
    assert!(s.spend_leases(false, None).unwrap().is_empty());
}

#[test]
fn re_registering_a_linked_resource_goes_through_the_gate() {
    let (s, _) = store();
    gpu_with_approval(&s);
    gpu_resource(&s, 86_400);
    let target = ResourceRef::parse("gpu:pod:a100").unwrap();
    s.link_resource_spend(&target, Some("gpu-h100"), None)
        .unwrap();
    // The owner releases: the resource lapses, still linked.
    s.release_resource("claude@shop", &target, false).unwrap();
    // Re-registering it to adopt asks for approval first, with no lease and no free use.
    let out = s
        .register_resource(RegisterResource {
            agent: "codex@shop".into(),
            kind: ResourceKind::Gpu,
            name: "pod:a100".into(),
            endpoint: None,
            port: None,
            pid: None,
            project: Some("shop".into()),
            purpose: None,
            mode: LeaseMode::Shared,
            ttl_seconds: Some(3600),
            metadata: None,
        })
        .unwrap();
    assert_eq!(out.outcome, "pending_approval");
    assert!(out.lease.is_none() && out.decision_id.is_some());
    assert_eq!(out.resource.refcount, 0);
    let pending = s.spend_leases(false, None).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].status, SpendLeaseStatus::PendingApproval);
    // Approved: the lease is taken and charged.
    s.answer_decision(
        AnswerDecision {
            decision: out.decision_id.unwrap(),
            value: Some("approve".into()),
            relayed_by: Some("manager".into()),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    assert!(s
        .resource(&target)
        .unwrap()
        .leases
        .iter()
        .any(|l| l.agent == "codex@shop"));
    assert_eq!(
        s.spend_leases(false, None).unwrap()[0].status,
        SpendLeaseStatus::Active
    );
}

#[test]
fn newly_started_leases_are_counted_before_the_clock_advances() {
    let (s, _) = store();
    account(&s, "cpu", SpendKind::Server, "USD", Some(100.0), |a| {
        a.cost_per_hour = Some(3.0)
    });
    account(&s, "total", SpendKind::Server, "USD", None, |a| {
        a.cost_per_hour = Some(3.0);
        a.period = SpendPeriod::None;
    });
    borrow(&s, "codex@shop", "cpu", None);
    borrow(&s, "codex@shop", "total", None);
    for period in ["current", "2026-11", "previous", "2026-12"] {
        let r = s
            .spend_report(
                SpendQuery {
                    period: period.parse().unwrap(),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        let cpu = find(&r, "cpu");
        let expected = u32::from(matches!(period, "current" | "2026-11"));
        assert_eq!(cpu.accrued, 0.0, "{period}");
        assert_eq!(cpu.active_leases, expected, "{period}");
        let total = find(&r, "total");
        assert_eq!((total.accrued, total.active_leases), (0.0, 1));
    }
}

#[test]
fn open_leases_at_a_monthly_reset_count_without_current_period_cost() {
    let (s, clock) = store();
    account(&s, "cpu", SpendKind::Server, "USD", Some(100.0), |a| {
        a.cost_per_hour = Some(3.0)
    });
    advance(&clock, 9 * 24 * HOUR + 23 * HOUR); // Nov 30 23:00
    s.borrow_spend(
        BorrowSpend {
            agent: "codex@shop".into(),
            account: "cpu".into(),
            ttl_seconds: Some(4 * 3600),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    advance(&clock, HOUR); // Dec 1 00:00, the selected period's start
    borrow(&s, "claude@shop", "cpu", None);
    for (period, accrued, active) in [
        ("current", 0.0, 2),
        ("2026-12", 0.0, 2),
        ("previous", 3.0, 1),
        ("2026-11", 3.0, 1),
        ("2027-01", 0.0, 0),
    ] {
        let r = s
            .spend_report(
                SpendQuery {
                    period: period.parse().unwrap(),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        let cpu = find(&r, "cpu");
        assert_eq!(
            (cpu.accrued, cpu.active_leases),
            (accrued, active),
            "{period}"
        );
    }
}

#[test]
fn open_leases_count_toward_earlier_periods_they_overlap() {
    let (s, clock) = store();
    account(&s, "cpu", SpendKind::Server, "USD", Some(100.0), |a| {
        a.cost_per_hour = Some(3.0)
    });
    // Nov 30 23:00, kept open past the Dec 1 reset (long TTL, no release).
    advance(&clock, 9 * 24 * HOUR + 23 * HOUR);
    s.borrow_spend(
        BorrowSpend {
            agent: "codex@shop".into(),
            account: "cpu".into(),
            ttl_seconds: Some(4 * 3600),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    advance(&clock, 2 * HOUR); // Dec 1 01:00
    let accrued = |period: &str| {
        let r = s
            .spend_report(
                SpendQuery {
                    period: period.parse().unwrap(),
                    account: Some("cpu".into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        find(&r, "cpu").accrued
    };
    assert_eq!(
        accrued("previous"),
        3.0,
        "November's hour, while still open"
    );
    assert_eq!(accrued("2026-11"), 3.0);
    assert_eq!(accrued("current"), 3.0, "December's hour");
    assert_eq!(accrued("2026-10"), 0.0, "before the lease");
}
