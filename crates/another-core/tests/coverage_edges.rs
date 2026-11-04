//! Edge cases for the least-covered public functions found by the v0.1.0 coverage pass
//! (`cargo llvm-cov`): the scheduled summary, spend account updates and status names,
//! remote-agent presence. Tests only; anything surprising is recorded in BUGS-FOUND.md.

use another_core::*;

type Handle = std::sync::Arc<std::sync::atomic::AtomicI64>;

const NOV_21: i64 = 1_795_219_200_000;
const HOUR: i64 = 3_600_000;

fn store() -> (Store, Handle) {
    let (clock, h) = Clock::manual(NOV_21);
    let s = Store::open_in_memory(clock).unwrap();
    s.ensure_project("t", "/work/shop").unwrap();
    s.ensure_project("t", "/work/vault").unwrap();
    for (name, program, project) in [
        ("jacob", Program::Human, None),
        ("claude", Program::Claude, Some("/work/shop")),
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
    (s, h)
}

fn advance(h: &Handle, ms: i64) {
    h.fetch_add(ms, std::sync::atomic::Ordering::SeqCst);
}

fn scope_for(s: &Store, agent: &str, glob: &str) -> TokenScope {
    let t = s
        .create_token(agent, &[glob.to_string()], None, None)
        .unwrap();
    s.verify_token(&t.secret).unwrap()
}

// ---------------------------------------------------------------------------------------
// Store::send_scheduled_summary (what `another serve` calls each sweep, in local time)

#[test]
fn scheduled_summary_sends_once_a_day_and_not_when_off() {
    let (s, h) = store();
    // Off by default: nothing to send.
    assert_eq!(s.send_scheduled_summary().unwrap(), None);

    // 00:00 local is always due: the first sweep sends it, by mail from the system agent.
    s.set_summary_schedule(Some("00:00"), Some("mail:jacob"))
        .unwrap();
    let sent = s.send_scheduled_summary().unwrap();
    assert_eq!(sent.as_deref(), Some("mail:jacob"), "{sent:?}");
    let inbox = s
        .inbox(
            "jacob",
            InboxQuery {
                limit: 50,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        inbox
            .messages
            .iter()
            .any(|m| m.message.subject.starts_with("[summary]")),
        "{:?}",
        inbox
            .messages
            .iter()
            .map(|m| &m.message.subject)
            .collect::<Vec<_>>()
    );

    // Later the same day: not again (several sweepers may share one home).
    advance(&h, HOUR);
    assert_eq!(s.send_scheduled_summary().unwrap(), None);
    assert!(s.summary_schedule().unwrap().last_sent_at.is_some());

    // Turned off: nothing, even the next day.
    s.set_summary_schedule(None, None).unwrap();
    advance(&h, 24 * HOUR);
    assert_eq!(s.send_scheduled_summary().unwrap(), None);
}

#[test]
fn scheduled_summary_settings_reject_malformed_values() {
    let (s, _) = store();
    for bad in ["25:00", "9:00", "09:60", "nine", "09:00:00"] {
        assert!(
            matches!(
                s.set_summary_schedule(Some(bad), None),
                Err(Error::Invalid(_))
            ),
            "{bad}"
        );
    }
    for bad in ["email:jacob", "mail:", "slack"] {
        assert!(
            matches!(
                s.set_summary_schedule(Some("08:30"), Some(bad)),
                Err(Error::Invalid(_))
            ),
            "{bad}"
        );
    }
}

#[test]
fn scheduled_summary_to_an_unknown_person_fails_every_sweep() {
    // BUGS-FOUND.md #1: `summary_to = mail:NAME` only checks the name's shape, so a
    // schedule to nobody is accepted and every sweep fails (never marked sent, retried).
    let (s, h) = store();
    s.set_summary_schedule(Some("00:00"), Some("mail:nobody"))
        .unwrap();
    assert!(matches!(
        s.send_scheduled_summary(),
        Err(Error::NotFound(_))
    ));
    advance(&h, 60_000);
    assert!(
        s.send_scheduled_summary().is_err(),
        "still failing on the next sweep"
    );
    assert_eq!(s.summary_schedule().unwrap().last_sent_at, None);
}

// ---------------------------------------------------------------------------------------
// Store::update_spend_account

fn gpu(s: &Store) -> SpendAccount {
    s.create_spend_account(
        NewSpendAccount {
            name: "gpu".into(),
            kind: SpendKind::GpuPod,
            unit: "USD".into(),
            budget: Some(100.0),
            period: SpendPeriod::Monthly,
            reset_day: 1,
            cost_per_hour: Some(2.0),
            requires_approval: false,
            approver: None,
            project: None,
            description: Some("pod".into()),
        },
        None,
    )
    .unwrap()
}

#[test]
fn spend_account_updates_validate_limits_and_keep_what_is_not_given() {
    let (s, _) = store();
    gpu(&s);
    let bad = |u: SpendAccountUpdate| {
        matches!(
            s.update_spend_account("gpu", u, None),
            Err(Error::Invalid(_))
        )
    };
    assert!(bad(SpendAccountUpdate {
        budget: Some(-1.0),
        ..Default::default()
    }));
    assert!(bad(SpendAccountUpdate {
        budget: Some(f64::NAN),
        ..Default::default()
    }));
    assert!(bad(SpendAccountUpdate {
        cost_per_hour: Some(f64::INFINITY),
        ..Default::default()
    }));
    for day in [0, 29, 31] {
        assert!(bad(SpendAccountUpdate {
            reset_day: Some(day),
            ..Default::default()
        }));
    }
    // A refused update changes nothing.
    let a = &s.spend_accounts(None).unwrap()[0];
    assert_eq!(
        (a.budget, a.cost_per_hour, a.reset_day),
        (Some(100.0), Some(2.0), 1)
    );

    // A partial update keeps the other fields; clear_budget removes the budget.
    let a = s
        .update_spend_account(
            "gpu",
            SpendAccountUpdate {
                reset_day: Some(28),
                requires_approval: Some(true),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    assert_eq!(a.reset_day, 28);
    assert!(a.requires_approval);
    assert_eq!((a.budget, a.cost_per_hour), (Some(100.0), Some(2.0)));
    assert_eq!(a.description.as_deref(), Some("pod"));
    let a = s
        .update_spend_account(
            "gpu",
            SpendAccountUpdate {
                clear_budget: true,
                ..Default::default()
            },
            None,
        )
        .unwrap();
    assert_eq!(a.budget, None);

    // Unknown accounts are not found.
    assert!(matches!(
        s.update_spend_account("nope", SpendAccountUpdate::default(), None),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn remote_tokens_cannot_change_spend_accounts() {
    let (s, _) = store();
    gpu(&s);
    let scope = scope_for(&s, "claude@shop", "/work/shop");
    assert!(matches!(
        s.update_spend_account(
            "gpu",
            SpendAccountUpdate {
                budget: Some(1_000_000.0),
                ..Default::default()
            },
            Some(&scope)
        ),
        Err(Error::Forbidden(_))
    ));
    assert_eq!(s.spend_accounts(None).unwrap()[0].budget, Some(100.0));
}

// ---------------------------------------------------------------------------------------
// SpendPeriod::as_str, SpendLeaseStatus::as_str

#[test]
fn spend_names_round_trip_and_malformed_ones_are_refused() {
    for p in [SpendPeriod::Monthly, SpendPeriod::None] {
        assert_eq!(p.as_str().parse::<SpendPeriod>().unwrap(), p);
    }
    assert_eq!(
        "lifetime".parse::<SpendPeriod>().unwrap(),
        SpendPeriod::None
    );
    assert!(matches!(
        "weekly".parse::<SpendPeriod>(),
        Err(Error::Invalid(_))
    ));
    let names: Vec<&str> = [
        SpendLeaseStatus::PendingApproval,
        SpendLeaseStatus::Active,
        SpendLeaseStatus::Ended,
        SpendLeaseStatus::Denied,
    ]
    .map(SpendLeaseStatus::as_str)
    .to_vec();
    assert_eq!(names, ["pending_approval", "active", "ended", "denied"]);
}

#[test]
fn lifetime_accounts_have_no_period_bounds_and_list_as_none() {
    let (s, h) = store();
    s.create_spend_account(
        NewSpendAccount {
            name: "domain".into(),
            kind: SpendKind::Domain,
            unit: "USD".into(),
            budget: Some(50.0),
            period: SpendPeriod::None,
            reset_day: 1,
            cost_per_hour: None,
            requires_approval: false,
            approver: None,
            project: None,
            description: None,
        },
        None,
    )
    .unwrap();
    s.record_spend(
        RecordSpend {
            agent: Some("claude@shop".into()),
            account: "domain".into(),
            amount: 12.0,
            project: Some("/work/shop".into()),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    // Months later the lifetime spend still counts: no monthly reset.
    advance(&h, 90 * 24 * HOUR);
    let r = s.spend_report(SpendQuery::default(), None).unwrap();
    let a = r
        .accounts
        .iter()
        .find(|a| a.account.name == "domain")
        .unwrap();
    assert_eq!(a.account.period, SpendPeriod::None);
    assert_eq!(a.spent, 12.0);
    assert_eq!(a.remaining, Some(38.0));
}

#[test]
fn pending_and_denied_leases_list_by_status() {
    let (s, _) = store();
    let mut a = NewSpendAccount {
        name: "gpu".into(),
        kind: SpendKind::GpuPod,
        unit: "USD".into(),
        budget: Some(100.0),
        period: SpendPeriod::Monthly,
        reset_day: 1,
        cost_per_hour: Some(2.0),
        requires_approval: true,
        approver: Some("jacob".into()),
        project: None,
        description: None,
    };
    a.requires_approval = true;
    s.create_spend_account(a, None).unwrap();
    let borrow = || {
        s.borrow_spend(
            BorrowSpend {
                agent: "claude@shop".into(),
                account: "gpu".into(),
                lane: None,
                cost_per_hour: None,
                ttl_seconds: None,
                hours: Some(1.0),
                note: None,
            },
            None,
        )
        .unwrap()
    };
    let pending = borrow();
    let denied = borrow();
    assert_eq!(pending.lease.status, SpendLeaseStatus::PendingApproval);
    s.answer_decision(
        AnswerDecision {
            decision: denied.decision_id.clone().unwrap(),
            value: Some("deny".into()),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let all = s.spend_leases(true, None).unwrap();
    let status = |id: &str| all.iter().find(|l| l.lease_id == id).unwrap().status;
    assert_eq!(
        status(&pending.lease.lease_id),
        SpendLeaseStatus::PendingApproval
    );
    assert_eq!(status(&denied.lease.lease_id), SpendLeaseStatus::Denied);
    assert_eq!(
        serde_json::to_value(status(&denied.lease.lease_id)).unwrap(),
        serde_json::json!("denied")
    );
}

// ---------------------------------------------------------------------------------------
// Store::remote_agents (presence)

#[test]
fn remote_agent_presence_goes_online_idle_and_gone() {
    let (s, h) = store();
    let invite = |name: &str, ephemeral: bool| {
        let code = s
            .create_agent_invite(NewAgentInvite {
                agent: name.into(),
                program: Program::Codex,
                project_globs: vec!["/work/shop".into()],
                lane: None,
                expires_in_seconds: None,
                max_uses: None,
                ephemeral,
                idle_hours: Some(2),
                token_ttl_seconds: None,
            })
            .unwrap()
            .code;
        s.redeem_invite(
            &code,
            JoinClient {
                hostname: Some("x".repeat(500)),
                version: None,
            },
        )
        .unwrap()
    };
    let cloud = invite("cloud", true);
    let desk = invite("desk", false);
    let presence = |name: &str| {
        s.remote_agents()
            .unwrap()
            .into_iter()
            .find(|r| r.agent.starts_with(name))
            .unwrap()
    };
    let p = presence("cloud");
    assert_eq!(p.presence, Presence::Online);
    assert_eq!(p.idle_hours, 2);
    assert!(
        p.client_hostname.as_deref().unwrap().chars().count() <= 100,
        "client-supplied hostnames are clipped"
    );

    // Past the online window but within idle_hours: idle, still there.
    advance(&h, remote_agents_online_window() + 60_000);
    assert_eq!(presence("cloud").presence, Presence::Idle);
    assert_eq!(presence("desk").presence, Presence::Idle);

    // Past idle_hours: the ephemeral agent is gone (token revoked); the persistent one stays.
    advance(&h, 2 * HOUR);
    let gone = presence("cloud");
    assert_eq!(gone.presence, Presence::Gone);
    assert!(
        gone.left_at.is_some() && gone.left_reason.is_some(),
        "{gone:?}"
    );
    assert!(s.verify_token(&cloud.token).is_err());
    assert_eq!(presence("desk").presence, Presence::Idle);
    assert!(s.verify_token(&desk.token).is_ok());
    // Using the token makes it online again.
    assert_eq!(presence("desk").presence, Presence::Online);
}

fn remote_agents_online_window() -> i64 {
    ONLINE_WINDOW_MS
}
