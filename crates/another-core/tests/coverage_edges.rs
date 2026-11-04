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
fn a_summary_to_an_unknown_person_is_never_recorded_as_sent() {
    // BUGS-FOUND.md #1: today `summary_to = mail:NAME` only checks the name's shape, so a
    // schedule to nobody is accepted and every sweep fails. PR #25 refuses it when set. What
    // holds on both sides of that fix: nothing is delivered, and nothing claims it was.
    let (s, h) = store();
    if s.set_summary_schedule(Some("00:00"), Some("mail:nobody"))
        .is_ok()
    {
        assert!(matches!(
            s.send_scheduled_summary(),
            Err(Error::NotFound(_))
        ));
        advance(&h, 60_000);
        assert!(
            s.send_scheduled_summary().is_err(),
            "still failing on the next sweep"
        );
    }
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

// =======================================================================================
// Second coverage pass (`cargo llvm-cov`): the next-lowest public functions. Tests only.

fn code<T: std::fmt::Debug>(r: Result<T>) -> String {
    r.unwrap_err().code().to_string()
}

fn message<T: std::fmt::Debug>(r: Result<T>) -> String {
    r.unwrap_err().to_string()
}

fn yes_no() -> Vec<DecisionOption> {
    ["yes", "no"]
        .map(|v| DecisionOption {
            value: v.into(),
            label: v.to_uppercase(),
            description: None,
        })
        .to_vec()
}

fn question() -> NewDecision {
    NewDecision {
        agent: "claude@shop".into(),
        question: "Ship it?".into(),
        options: yes_no(),
        ..Default::default()
    }
}

fn lane(s: &Store, name: &str, project: &str, lead: &str) {
    s.create_lane(
        NewLane {
            agent: "jacob".into(),
            name: name.into(),
            project: project.into(),
            lead: lead.into(),
            charter: String::new(),
            status: Some(LaneStatus::Working),
        },
        None,
    )
    .unwrap();
}

fn vaulter(s: &Store) {
    s.register_agent_trusted(RegisterAgent {
        name: "vaulter".into(),
        program: Program::Codex,
        project: Some("/work/vault".into()),
        model: None,
        description: None,
    })
    .unwrap();
}

// ---------------------------------------------------------------------------------------
// Store::ask_decision, answer_decision, cancel_decision, decisions, decision_markdown

#[test]
fn decisions_refuse_a_self_asked_system_or_overlinked_question() {
    let (s, _) = store();
    // Nobody can be asked to decide for themselves, nor the system agent.
    let mut q = question();
    q.agent = "jacob".into();
    q.human = Some("jacob".into());
    assert!(message(s.ask_decision(q, None)).contains("cannot ask itself"));
    let mut q = question();
    q.human = Some(SYSTEM_AGENT.into());
    assert!(message(s.ask_decision(q, None)).contains("not the system agent"));

    // The context link limit is 20: exactly that many is fine.
    let links = |n: usize| {
        (0..n)
            .map(|i| format!("https://example.com/{i}"))
            .collect::<Vec<_>>()
    };
    let mut q = question();
    q.context_links = links(21);
    assert!(message(s.ask_decision(q, None)).contains("at most 20"));
    let mut q = question();
    q.human = Some("jacob".into());
    q.context_links = links(20);
    let d = s.ask_decision(q, None).unwrap();
    assert_eq!(d.context_links.len(), 20);

    // A closed lane takes no new questions, and refusing leaves no decision behind.
    lane(&s, "api", "/work/shop", "claude@shop");
    s.close_lane("jacob", "api", true, None, None).unwrap();
    let mut q = question();
    q.human = Some("jacob".into());
    q.lane = Some("api".into());
    assert_eq!(code(s.ask_decision(q, None)), "conflict");
    assert_eq!(
        s.decisions(DecisionFilter::default(), None).unwrap().len(),
        1
    );
}

#[test]
fn decision_names_parse_their_aliases_and_refuse_the_rest() {
    for (raw, want) in [
        ("default", DecisionTimeout::ChooseDefault),
        ("choose_default", DecisionTimeout::ChooseDefault),
        ("  cancel ", DecisionTimeout::Cancel),
        ("wait", DecisionTimeout::KeepOpen),
        ("keep_open", DecisionTimeout::KeepOpen),
    ] {
        assert_eq!(raw.parse::<DecisionTimeout>().unwrap(), want, "{raw}");
    }
    for bad in ["", "later", "CANCEL"] {
        assert!(bad.parse::<DecisionTimeout>().is_err(), "{bad:?}");
        assert!(bad.parse::<DecisionStatus>().is_err(), "{bad:?}");
    }
    for st in [
        DecisionStatus::Open,
        DecisionStatus::Answered,
        DecisionStatus::Expired,
        DecisionStatus::Cancelled,
        DecisionStatus::AnswerSent,
    ] {
        assert_eq!(st.as_str().parse::<DecisionStatus>().unwrap(), st);
    }
}

#[test]
fn answers_need_something_to_say_and_only_count_once() {
    let (s, _) = store();
    let mut q = question();
    q.human = Some("jacob".into());
    let d = s.ask_decision(q.clone(), None).unwrap();
    let answer = |id: &str, value: Option<&str>, text: Option<&str>| {
        s.answer_decision(
            AnswerDecision {
                decision: id.into(),
                value: value.map(Into::into),
                text: text.map(Into::into),
                ..Default::default()
            },
            None,
        )
    };
    // Nothing, or only blanks; free text where it isn't allowed; a value that isn't an option.
    assert!(message(answer(&d.decision_id, None, None)).contains("give an option value"));
    assert!(message(answer(&d.decision_id, Some("  "), Some(" "))).contains("give an option"));
    assert!(message(answer(&d.decision_id, None, Some("why not"))).contains("free text"));
    assert_eq!(code(answer(&d.decision_id, Some("maybe"), None)), "invalid");
    assert_eq!(code(answer(&d.decision_id, Some("0"), None)), "invalid");
    assert_eq!(code(answer(&d.decision_id, Some("3"), None)), "invalid");
    assert_eq!(
        s.decision(&d.decision_id, None).unwrap().status,
        DecisionStatus::Open,
        "refused answers change nothing"
    );

    // A number picks an option; a note may go with it. A second answer is refused and the
    // first stands, even when it's the same answer again.
    let done = answer(&d.decision_id, Some("2"), Some("next week")).unwrap();
    let a = done.answer.clone().unwrap();
    assert_eq!(a.value.as_deref(), Some("no"));
    assert_eq!(a.text.as_deref(), Some("next week"));
    assert_eq!(code(answer(&d.decision_id, Some("2"), None)), "conflict");
    assert_eq!(code(answer(&d.decision_id, Some("yes"), None)), "conflict");
    assert_eq!(
        s.decision(&d.decision_id, None)
            .unwrap()
            .answer
            .unwrap()
            .value
            .as_deref(),
        Some("no")
    );

    // Free text alone is fine when allowed; the message asks for either.
    q.allow_free_text = true;
    let free = s.ask_decision(q, None).unwrap();
    assert!(message(answer(&free.decision_id, None, None)).contains("option value or free text"));
    let done = answer(&free.decision_id, None, Some("half of it")).unwrap();
    let md = decision_markdown(&done);
    assert!(md.contains("**Answer:** (text): half of it"), "{md}");

    // Unknown ids.
    assert_eq!(code(answer("nope", Some("yes"), None)), "not_found");
}

#[test]
fn cancelling_is_for_the_asker_while_the_decision_is_open() {
    let (s, _) = store();
    let mut q = question();
    q.human = Some("jacob".into());
    let d = s.ask_decision(q, None).unwrap();
    assert_eq!(
        code(s.cancel_decision("jacob", &d.decision_id, None, None)),
        "forbidden"
    );
    assert_eq!(
        code(s.cancel_decision("claude@shop", "nope", None, None)),
        "not_found"
    );
    let gone = s
        .cancel_decision("claude@shop", &d.decision_id, Some("moot".into()), None)
        .unwrap();
    assert_eq!(gone.status, DecisionStatus::Cancelled);
    // Twice, or answering afterwards: refused, and it stays cancelled.
    assert_eq!(
        code(s.cancel_decision("claude@shop", &d.decision_id, None, None)),
        "conflict"
    );
    assert_eq!(
        code(s.answer_decision(
            AnswerDecision {
                decision: d.decision_id.clone(),
                value: Some("yes".into()),
                ..Default::default()
            },
            None,
        )),
        "conflict"
    );
    // Closed with no answer: the markdown offers no way to reply.
    let md = decision_markdown(&s.decision(&d.decision_id, None).unwrap());
    assert!(md.contains("(cancelled)"), "{md}");
    assert!(
        !md.contains("Reply with") && !md.contains("**Answer:**"),
        "{md}"
    );
}

#[test]
fn decision_markdown_shows_descriptions_links_and_deadlines() {
    let (s, _) = store();
    let mut q = question();
    q.human = Some("jacob".into());
    q.options[0].description = Some("today".into());
    q.allow_free_text = true;
    q.context_links = vec!["https://example.com/pr/1".into()];
    q.deadline_at = Some(s.now() + HOUR);
    q.on_timeout = DecisionTimeout::Cancel;
    let d = s.ask_decision(q, None).unwrap();
    let md = decision_markdown(&d);
    assert!(md.contains("1. YES (`yes`) — today\n"), "{md}");
    assert!(md.contains("2. NO (`no`)\n"), "{md}");
    assert!(md.contains("Free text is OK too."), "{md}");
    assert!(md.contains("- https://example.com/pr/1\n"), "{md}");
    assert!(md.contains("Deadline "), "{md}");
    assert!(md.contains("Reply with a number or value"), "{md}");
    assert!(!md.contains("· lane"), "{md}");
}

#[test]
fn decision_lists_filter_within_the_callers_scope() {
    let (s, _) = store();
    vaulter(&s);
    lane(&s, "api", "/work/shop", "claude@shop");
    lane(&s, "keys", "/work/vault", "vaulter@vault");
    let ask = |agent: &str, lane: Option<&str>| {
        s.ask_decision(
            NewDecision {
                agent: agent.into(),
                human: Some("jacob".into()),
                lane: lane.map(Into::into),
                ..question()
            },
            None,
        )
        .unwrap()
    };
    let shop = ask("claude@shop", Some("api"));
    let vault = ask("vaulter@vault", Some("keys"));
    let ids = |f: DecisionFilter, scope: Option<&TokenScope>| {
        s.decisions(f, scope)
            .unwrap()
            .into_iter()
            .map(|d| d.decision_id)
            .collect::<Vec<_>>()
    };
    let token = scope_for(&s, "claude@shop", "/work/shop");
    // By project, by lane, by person; a token sees its own project only.
    assert_eq!(
        ids(
            DecisionFilter {
                project: Some("vault".into()),
                ..Default::default()
            },
            None
        ),
        std::slice::from_ref(&vault.decision_id)
    );
    assert_eq!(
        ids(
            DecisionFilter {
                lane: Some("api".into()),
                ..Default::default()
            },
            None
        ),
        std::slice::from_ref(&shop.decision_id)
    );
    assert!(ids(
        DecisionFilter {
            human: Some("nobody".into()),
            ..Default::default()
        },
        None
    )
    .is_empty());
    assert_eq!(
        ids(DecisionFilter::default(), Some(&token)),
        std::slice::from_ref(&shop.decision_id)
    );
    // Hidden and missing names give the same error: the message can't tell them apart.
    let err = |p: &str| {
        s.decisions(
            DecisionFilter {
                project: Some(p.into()),
                ..Default::default()
            },
            Some(&token),
        )
        .unwrap_err()
        .code()
    };
    assert_eq!(err("vault"), err("nowhere"));
    let lane_err = s
        .decisions(
            DecisionFilter {
                lane: Some("keys".into()),
                ..Default::default()
            },
            Some(&token),
        )
        .unwrap_err();
    assert_eq!(lane_err.code(), "not_found");
    assert_eq!(
        code(s.decision(&vault.decision_id, Some(&token))),
        "not_found"
    );
    assert_eq!(code(s.decision("nope", Some(&token))), "not_found");

    // Status filters; an answered decision leaves the default (open) list.
    s.answer_decision(
        AnswerDecision {
            decision: shop.decision_id.clone(),
            value: Some("yes".into()),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    assert!(ids(DecisionFilter::default(), Some(&token)).is_empty());
    assert_eq!(
        ids(
            DecisionFilter {
                status: Some(DecisionStatus::Answered),
                ..Default::default()
            },
            Some(&token)
        ),
        std::slice::from_ref(&shop.decision_id)
    );
    let all = ids(
        DecisionFilter {
            all: true,
            limit: 1,
            ..Default::default()
        },
        None,
    );
    assert_eq!(all.len(), 1, "limit applies across statuses");
}

// ---------------------------------------------------------------------------------------
// Store::create_spend_account, archive, record, borrow, release, leases, report

fn new_account(name: &str, unit: &str) -> NewSpendAccount {
    NewSpendAccount {
        name: name.into(),
        kind: SpendKind::ApiCredits,
        unit: unit.into(),
        budget: None,
        period: SpendPeriod::Monthly,
        reset_day: 1,
        cost_per_hour: Some(2.0),
        requires_approval: false,
        approver: None,
        project: None,
        description: None,
    }
}

fn borrow(agent: &str, account: &str) -> BorrowSpend {
    BorrowSpend {
        agent: agent.into(),
        account: account.into(),
        ..Default::default()
    }
}

#[test]
fn spend_account_names_and_units_are_validated() {
    let (s, _) = store();
    let token = scope_for(&s, "claude@shop", "/work/shop");
    for bad in ["", "   ", "has space", "semi;colon", &"x".repeat(65)] {
        assert_eq!(
            code(s.create_spend_account(new_account(bad, "USD"), None)),
            "invalid",
            "{bad:?}"
        );
    }
    for bad in ["", "   ", &"u".repeat(33)] {
        assert!(
            message(s.create_spend_account(new_account("ok", bad), None)).contains("unit must be"),
            "{bad:?}"
        );
    }
    // Limits are inclusive; names are case-folded and unique, and a duplicate changes nothing.
    let a = s
        .create_spend_account(new_account(&"N".repeat(64), &"u".repeat(32)), None)
        .unwrap();
    assert_eq!(a.name, "n".repeat(64));
    assert_eq!(
        code(s.create_spend_account(new_account(&"n".repeat(64), "USD"), None)),
        "conflict"
    );
    assert!(s
        .create_spend_account(new_account("remote", "USD"), Some(&token))
        .is_err());
    assert_eq!(s.spend_accounts(None).unwrap().len(), 1);
}

#[test]
fn spend_periods_refuse_malformed_selections() {
    for bad in [
        "2026", "2026-13", "2026-00", "1969-12", "10000-01", "abc-01", "2026-ab", "-", "2026-",
    ] {
        assert!(bad.parse::<PeriodSel>().is_err(), "{bad:?}");
    }
    for good in [
        "", " ", "current", "previous", "last", "2026-11", "1970-01", "9999-12",
    ] {
        assert!(good.parse::<PeriodSel>().is_ok(), "{good:?}");
    }
}

#[test]
fn spend_amounts_must_be_real_and_attributed_within_scope() {
    let (s, _) = store();
    vaulter(&s);
    lane(&s, "keys", "/work/vault", "vaulter@vault");
    s.create_spend_account(new_account("api", "USD"), None)
        .unwrap();
    let mut pinned = new_account("shop-api", "USD");
    pinned.project = Some("/work/shop".into());
    s.create_spend_account(pinned, None).unwrap();
    let rec = |amount: f64| RecordSpend {
        account: "api".into(),
        amount,
        ..Default::default()
    };
    for bad in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(code(s.record_spend(rec(bad), None)), "invalid", "{bad}");
    }

    // A token must attribute spend to a project in its globs.
    let token = scope_for(&s, "claude@shop", "/work/shop");
    assert_eq!(
        code(s.record_spend(rec(1.0), Some(&token))),
        "forbidden",
        "no project named"
    );
    assert_eq!(
        code(s.record_spend(
            RecordSpend {
                project: Some("/work/vault".into()),
                ..rec(1.0)
            },
            Some(&token)
        )),
        "not_found",
        "a hidden project looks missing"
    );
    let own = s
        .record_spend(
            RecordSpend {
                project: Some("/work/shop".into()),
                ..rec(1.0)
            },
            Some(&token),
        )
        .unwrap();
    assert_eq!(own.amount, 1.0);

    // A lane or project that disagrees with the account's own project, or with each other.
    let on_shop = |f: fn(&mut RecordSpend)| {
        let mut r = RecordSpend {
            account: "shop-api".into(),
            amount: 1.0,
            ..Default::default()
        };
        f(&mut r);
        s.record_spend(r, None)
    };
    assert!(message(on_shop(|r| r.lane = Some("keys@vault".into())))
        .contains("the lane belongs to a different project than the account"));
    assert!(message(on_shop(|r| r.project = Some("/work/vault".into())))
        .contains("the project belongs to a different project than the account"));
    assert!(message(s.record_spend(
        RecordSpend {
            lane: Some("keys@vault".into()),
            project: Some("/work/shop".into()),
            ..rec(1.0)
        },
        None
    ))
    .contains("the lane belongs to a different project"));

    // The report's lane filter, and refused spend didn't count.
    let by_lane = |lane: &str| {
        s.spend_report(
            SpendQuery {
                lane: Some(lane.into()),
                ..Default::default()
            },
            None,
        )
    };
    s.record_spend(
        RecordSpend {
            lane: Some("keys@vault".into()),
            ..rec(5.0)
        },
        None,
    )
    .unwrap();
    let r = by_lane("keys@vault").unwrap();
    let spent: f64 = r.accounts.iter().map(|a| a.spent).sum();
    assert_eq!(spent, 5.0, "{r:?}");
    assert_eq!(code(by_lane("nope")), "not_found");
}

#[test]
fn spend_leases_end_once_and_block_archiving_until_they_do() {
    let (s, h) = store();
    s.create_spend_account(new_account("gpu", "USD"), None)
        .unwrap();
    let out = s.borrow_spend(borrow("claude@shop", "gpu"), None).unwrap();
    let id = out.lease.lease_id.clone();
    assert_eq!(out.lease.status, SpendLeaseStatus::Active);
    assert_eq!(out.decision_id, None);

    // The account can't be archived from under an open lease.
    assert!(message(s.archive_spend_account("gpu", None)).contains("1 open lease"));
    assert_eq!(s.spend_accounts(None).unwrap().len(), 1);

    // Someone else's lease; an unknown one; then released once.
    assert_eq!(code(s.release_spend("jacob", &id, None)), "forbidden");
    assert_eq!(
        code(s.release_spend("claude@shop", "nope", None)),
        "not_found"
    );
    advance(&h, HOUR / 2);
    let ended = s.release_spend("claude@shop", &id, None).unwrap();
    assert_eq!(ended.status, SpendLeaseStatus::Ended);
    assert!(message(s.release_spend("claude@shop", &id, None)).contains("is already ended"));

    // A lease left to run out ends by itself at its TTL: releasing it then is a double end.
    let lapsed = s.borrow_spend(borrow("claude@shop", "gpu"), None).unwrap();
    advance(&h, HOUR);
    assert!(s.spend_leases(false, None).unwrap().is_empty());
    assert_eq!(
        code(s.release_spend("claude@shop", &lapsed.lease.lease_id, None)),
        "conflict"
    );
    s.spend_leases(true, None)
        .unwrap()
        .into_iter()
        .find(|l| l.lease_id == lapsed.lease.lease_id)
        .filter(|l| l.status == SpendLeaseStatus::Ended)
        .expect("ended by the sweep");

    // Open list versus everything; a token only sees its own project's leases.
    assert_eq!(s.spend_leases(true, None).unwrap().len(), 2);
    let vault_token = {
        vaulter(&s);
        scope_for(&s, "vaulter@vault", "/work/vault")
    };
    assert!(s.spend_leases(true, Some(&vault_token)).unwrap().is_empty());
    assert_eq!(
        code(s.release_spend("vaulter@vault", &id, Some(&vault_token))),
        "not_found",
        "hidden looks missing"
    );

    // Archiving works now, once; an archived account is gone from lists and lookups.
    let a = s.archive_spend_account("gpu", None).unwrap();
    assert_eq!(a.name, "gpu");
    assert!(s.spend_accounts(None).unwrap().is_empty());
    assert_eq!(code(s.archive_spend_account("gpu", None)), "not_found");
    assert_eq!(code(s.archive_spend_account("nope", None)), "not_found");
    assert!(s.archive_spend_account("gpu", Some(&vault_token)).is_err());
}

#[test]
fn spend_lease_hours_and_scope_are_checked_before_borrowing() {
    let (s, _) = store();
    vaulter(&s);
    lane(&s, "keys", "/work/vault", "vaulter@vault");
    s.create_spend_account(new_account("gpu", "USD"), None)
        .unwrap();
    for bad in [-1.0, f64::NAN, f64::INFINITY] {
        let mut b = borrow("claude@shop", "gpu");
        b.hours = Some(bad);
        assert_eq!(code(s.borrow_spend(b, None)), "invalid", "{bad}");
    }
    // A token with no project for the lease: `jacob` has none, and an agent's token can't name
    // a lane outside its globs.
    let token = scope_for(&s, "jacob", "/work/shop");
    assert_eq!(
        code(s.borrow_spend(borrow("jacob", "gpu"), Some(&token))),
        "forbidden"
    );
    let mut b = borrow("jacob", "gpu");
    b.lane = Some("keys@vault".into());
    assert!(s.borrow_spend(b, Some(&token)).is_err());
    assert!(s.spend_leases(true, None).unwrap().is_empty());

    // An approval has nowhere to go without a project.
    let mut gated = new_account("gated", "USD");
    gated.requires_approval = true;
    gated.approver = Some("jacob".into());
    s.create_spend_account(gated, None).unwrap();
    s.register_agent_trusted(RegisterAgent {
        name: "drifter".into(),
        program: Program::Other,
        project: None,
        model: None,
        description: None,
    })
    .unwrap();
    assert!(message(s.borrow_spend(borrow("drifter", "gated"), None))
        .contains("approval needs a project"));
    assert!(
        s.spend_leases(true, None).unwrap().is_empty(),
        "rolled back"
    );
}

// ---------------------------------------------------------------------------------------
// Store::create_agent_invite, revoke_agent_invite, redeem_invite, kick_agent_as

fn invite(agent: &str) -> NewAgentInvite {
    NewAgentInvite {
        agent: agent.into(),
        program: Program::Other,
        project_globs: vec!["/work/shop".into()],
        lane: None,
        expires_in_seconds: None,
        max_uses: None,
        ephemeral: false,
        idle_hours: None,
        token_ttl_seconds: None,
    }
}

#[test]
fn invites_validate_their_limits_at_the_edges() {
    let (s, _) = store();
    let refused = |f: fn(&mut NewAgentInvite)| {
        let mut i = invite("guest");
        f(&mut i);
        message(s.create_agent_invite(i))
    };
    assert!(refused(|i| i.idle_hours = Some(0)).contains("--idle-hours"));
    assert!(refused(|i| i.idle_hours = Some(721)).contains("--idle-hours"));
    assert!(refused(|i| i.token_ttl_seconds = Some(59)).contains("token ttl"));
    assert!(refused(|i| i.expires_in_seconds = Some(59)).contains("invite expiry"));
    assert!(refused(|i| i.max_uses = Some(0)).contains("--uses"));
    assert!(refused(|i| i.max_uses = Some(101)).contains("--uses"));
    assert!(refused(|i| i.project_globs = vec!["  ".into()]).contains("--project"));
    assert!(refused(|i| i.project_globs = vec!["/work/[".into()]).contains("invalid project glob"));
    assert!(refused(|i| i.agent = "claude".into()).contains("already registered"));
    assert!(s.agent_invites().unwrap().is_empty(), "nothing was created");

    // Each limit itself is accepted.
    let mut i = invite("guest");
    i.idle_hours = Some(720);
    i.token_ttl_seconds = Some(60);
    i.expires_in_seconds = Some(60);
    i.max_uses = Some(100);
    s.create_agent_invite(i).unwrap();
    assert_eq!(s.agent_invites().unwrap().len(), 1);
}

#[test]
fn an_invite_for_a_lane_needs_it_open_and_stays_redeemable_only_while_it_is() {
    let (s, _) = store();
    lane(&s, "api", "/work/shop", "claude@shop");
    let mut i = invite("guest");
    i.lane = Some("api".into());
    let made = s.create_agent_invite(i.clone()).unwrap();

    // The lane closes between the invite and its use: the same refusal as any bad code.
    s.close_lane("jacob", "api", true, None, None).unwrap();
    assert_eq!(
        code(s.redeem_invite(&made.code, JoinClient::default())),
        "forbidden"
    );
    assert_eq!(s.agent_invites().unwrap()[0].uses, 0);
    // And no new invites for it.
    i.agent = "guest2".into();
    assert!(message(s.create_agent_invite(i)).contains("is closed"));
}

#[test]
fn revoking_an_invite_is_idempotent_and_ends_redemption() {
    let (s, h) = store();
    assert_eq!(code(s.revoke_agent_invite("nope")), "not_found");
    let made = s.create_agent_invite(invite("guest")).unwrap();
    let id = made.invite.invite_id.clone();
    // Ids are normalized, so a lower-case copy works.
    let first = s.revoke_agent_invite(&id.to_ascii_lowercase()).unwrap();
    let at = first.revoked_at.clone().unwrap();
    advance(&h, HOUR);
    let again = s.revoke_agent_invite(&id).unwrap();
    assert_eq!(
        again.revoked_at.as_deref(),
        Some(at.as_str()),
        "keeps the first time"
    );
    assert_eq!(
        code(s.redeem_invite(&made.code, JoinClient::default())),
        "forbidden"
    );
    assert_eq!(s.agent_invites().unwrap()[0].revoked_at, Some(at));
}

#[test]
fn malformed_invite_codes_all_get_the_same_refusal() {
    let (s, _) = store();
    let made = s.create_agent_invite(invite("guest")).unwrap();
    let wrong_secret = {
        let (head, _) = made.code.rsplit_once('_').unwrap();
        format!("{head}_{}", "0".repeat(48))
    };
    let refusal = message(s.redeem_invite(&wrong_secret, JoinClient::default()));
    for bad in [
        "",
        "  ",
        "anj_",
        "anj_short_secret",
        "not-a-code",
        "anj_\u{1F980}_x",
    ] {
        assert_eq!(invite_id_of(bad), None, "{bad:?}");
        assert_eq!(
            message(s.redeem_invite(bad, JoinClient::default())),
            refusal,
            "{bad:?}"
        );
    }
    let empty_secret = format!("{}_", made.code.rsplit_once('_').unwrap().0);
    assert_eq!(invite_id_of(&empty_secret), None);
    assert_eq!(s.agent_invites().unwrap()[0].uses, 0);
    // Whitespace around a good code is forgiven; using it up then refuses the same way.
    s.redeem_invite(&format!(" {} ", made.code), JoinClient::default())
        .unwrap();
    assert_eq!(
        message(s.redeem_invite(&made.code, JoinClient::default())),
        refusal,
        "a double redeem"
    );
    assert_eq!(s.agent_invites().unwrap()[0].uses, 1);
}

#[test]
fn managers_kick_within_their_scope_and_not_themselves_or_people() {
    let (s, _) = store();
    vaulter(&s);
    s.register_agent_trusted(RegisterAgent {
        name: "boss".into(),
        program: Program::GrokBot,
        project: Some("/work/shop".into()),
        model: None,
        description: None,
    })
    .unwrap();
    let token = scope_for(&s, "boss@shop", "/work/shop");
    let other = scope_for(&s, "claude@shop", "/work/shop");
    let kick = |caller: &str, target: &str, scope| s.kick_agent_as(caller, target, None, scope);

    assert_eq!(code(kick("nobody", "claude@shop", None)), "not_found");
    assert_eq!(code(kick("claude@shop", "boss@shop", None)), "forbidden");
    assert_eq!(
        code(kick("boss@shop", "claude@shop", Some(&other))),
        "forbidden",
        "acting as someone else's token"
    );
    assert!(message(kick("boss@shop", "boss@shop", None)).contains("use leave"));
    assert_eq!(code(kick("boss@shop", "jacob", None)), "forbidden");
    // The system agent exists once it has sent something.
    assert_eq!(code(kick("boss@shop", SYSTEM_AGENT, None)), "not_found");
    s.set_summary_schedule(Some("00:00"), Some("mail:jacob"))
        .unwrap();
    s.send_scheduled_summary().unwrap();
    assert_eq!(code(kick("boss@shop", SYSTEM_AGENT, None)), "forbidden");
    // Outside the token's projects, and an agent with no project: both look missing.
    let hidden = message(kick("boss@shop", "vaulter@vault", Some(&token)));
    let missing = message(kick("boss@shop", "nobody", Some(&token)));
    assert!(hidden.contains("unknown agent") && missing.contains("unknown agent"));
    assert_eq!(code(kick("boss@shop", "jacob", Some(&token))), "not_found");

    let d = s
        .kick_agent_as("boss@shop", "claude@shop", Some("done here"), Some(&token))
        .unwrap();
    assert_eq!(d.reason, "kicked: done here");
    assert!(d.tokens_revoked >= 1, "{d:?}");
    // The local kick: unknown names; a kicked agent can be kicked again without harm.
    assert_eq!(code(s.kick_agent("nobody", None)), "not_found");
    let again = s.kick_agent("claude@shop", None).unwrap();
    assert_eq!(again.tokens_revoked, 0, "{again:?}");
}

// ---------------------------------------------------------------------------------------
// Store::daily_summary (attention), set_summary_schedule

#[test]
fn the_summary_flags_overpace_budgets_and_stale_requests() {
    let (s, h) = store();
    // Nov 21 of a month starting on the 1st: 79 spent projects past a budget of 100, while
    // still under the 80% warning line by amount.
    let mut a = new_account("api", "USD");
    a.budget = Some(100.0);
    a.project = Some("/work/shop".into());
    s.create_spend_account(a, None).unwrap();
    s.record_spend(
        RecordSpend {
            account: "api".into(),
            amount: 79.0,
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let sum = s.daily_summary(None, None).unwrap();
    assert!(
        sum.attention
            .iter()
            .any(|l| l.starts_with("api is on pace to exceed its budget")),
        "{:?}",
        sum.attention
    );
    assert!(sum.attention.iter().all(|l| !l.contains("is over budget")));

    // A request past its due time.
    s.send_message(SendMessage {
        from: "claude@shop".into(),
        to: vec!["jacob".into()],
        subject: "Look at this".into(),
        body: String::new(),
        request: Some(NewRequest {
            due_in_seconds: Some(60),
        }),
        ..Default::default()
    })
    .unwrap();
    advance(&h, HOUR);
    let sum = s.daily_summary(None, None).unwrap();
    assert!(
        sum.attention.iter().any(|l| l == "1 stale request(s)"),
        "{:?}",
        sum.attention
    );
}

#[test]
fn summary_times_refuse_what_is_not_digits() {
    let (s, _) = store();
    for bad in ["ab:00", "09:xx", "::", ":", "-1:00", "09:-1"] {
        assert_eq!(
            code(s.set_summary_schedule(Some(bad), None)),
            "invalid",
            "{bad:?}"
        );
    }
    assert_eq!(s.summary_schedule().unwrap().daily_at, None);
}

// ---------------------------------------------------------------------------------------
// Store::link_resource_spend

#[test]
fn linking_a_resource_needs_a_priced_account_and_stays_local() {
    let (s, _) = store();
    let token = scope_for(&s, "claude@shop", "/work/shop");
    s.register_resource(RegisterResource {
        agent: "claude@shop".into(),
        kind: ResourceKind::DevServer,
        name: "pod".into(),
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
    let pod = ResourceRef::parse("dev_server:pod").unwrap();
    let mut free = new_account("free", "USD");
    free.cost_per_hour = None;
    s.create_spend_account(free, None).unwrap();
    gpu(&s);

    assert!(message(s.link_resource_spend(&pod, Some("free"), None)).contains("no cost_per_hour"));
    assert_eq!(
        code(s.link_resource_spend(&pod, Some("nope"), None)),
        "not_found"
    );
    let missing = ResourceRef::parse("dev_server:nope").unwrap();
    assert_eq!(
        code(s.link_resource_spend(&missing, Some("gpu"), None)),
        "not_found"
    );
    assert!(s
        .link_resource_spend(&pod, Some("gpu"), Some(&token))
        .is_err());

    // Linking twice is the same as once, and unlinking with no link is harmless.
    let id = s
        .link_resource_spend(&pod, Some("gpu"), None)
        .unwrap()
        .resource_id;
    assert_eq!(
        s.link_resource_spend(&pod, Some("gpu"), None)
            .unwrap()
            .resource_id,
        id
    );
    s.link_resource_spend(&pod, None, None).unwrap();
    s.link_resource_spend(&pod, None, None).unwrap();
}

// ---------------------------------------------------------------------------------------
// Real double runs: several connections to one home at once, as `serve` and stdio MCP servers
// share it. Exactly one must win, and the losers must get a clean refusal.

const RACERS: usize = 8;

/// Run `f` on `RACERS` threads, each with its own connection, all released together.
fn race<T: Send>(home: &std::path::Path, f: impl Fn(&Store) -> T + Sync) -> Vec<T> {
    let gate = std::sync::Barrier::new(RACERS);
    std::thread::scope(|t| {
        let runs: Vec<_> = (0..RACERS)
            .map(|_| {
                t.spawn(|| {
                    let s = Store::open(home).unwrap();
                    gate.wait();
                    f(&s)
                })
            })
            .collect();
        runs.into_iter().map(|r| r.join().unwrap()).collect()
    })
}

fn home_with_agents() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path()).unwrap();
    s.ensure_project("t", "/work/shop").unwrap();
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
    (dir, s)
}

#[test]
fn racing_sweeps_send_the_daily_summary_once() {
    let (dir, s) = home_with_agents();
    s.set_summary_schedule(Some("00:00"), Some("mail:jacob"))
        .unwrap();
    let sent = race(dir.path(), |s| s.send_scheduled_summary().unwrap());
    assert_eq!(sent.iter().filter(|x| x.is_some()).count(), 1, "{sent:?}");
    let inbox = s
        .inbox(
            "jacob",
            InboxQuery {
                limit: 50,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(inbox.messages.len(), 1, "{inbox:?}");
}

#[test]
fn racing_redemptions_stop_at_the_invites_limit() {
    let (dir, s) = home_with_agents();
    let made = s
        .create_agent_invite(NewAgentInvite {
            max_uses: Some(3),
            ..invite("guest")
        })
        .unwrap();
    let joined = race(dir.path(), |s| {
        s.redeem_invite(&made.code, JoinClient::default())
            .map(|j| j.agent)
            .map_err(|e| e.code().to_string())
    });
    let won: Vec<_> = joined.iter().filter_map(|j| j.as_ref().ok()).collect();
    assert_eq!(won.len(), 3, "{joined:?}");
    assert!(
        joined
            .iter()
            .filter_map(|j| j.as_ref().err())
            .all(|e| e == "forbidden"),
        "{joined:?}"
    );
    assert_eq!(s.agent_invites().unwrap()[0].uses, 3);
}

#[test]
fn racing_answers_and_releases_take_effect_once() {
    let (dir, s) = home_with_agents();
    let d = s
        .ask_decision(
            NewDecision {
                human: Some("jacob".into()),
                ..question()
            },
            None,
        )
        .unwrap();
    let values = std::sync::atomic::AtomicUsize::new(0);
    let answers = race(dir.path(), |s| {
        // Half say yes and half no: the stored answer must be the one winner's.
        let i = values.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let value = ["yes", "no"][i % 2];
        s.answer_decision(
            AnswerDecision {
                decision: d.decision_id.clone(),
                value: Some(value.into()),
                ..Default::default()
            },
            None,
        )
        .map(|_| value)
        .map_err(|e| e.code().to_string())
    });
    let won: Vec<_> = answers.iter().filter_map(|a| a.as_ref().ok()).collect();
    assert_eq!(won.len(), 1, "{answers:?}");
    assert!(
        answers
            .iter()
            .filter_map(|a| a.as_ref().err())
            .all(|e| e == "conflict"),
        "{answers:?}"
    );
    let stored = s.decision(&d.decision_id, None).unwrap().answer.unwrap();
    assert_eq!(stored.value.as_deref(), Some(*won[0]));

    s.create_spend_account(new_account("gpu", "USD"), None)
        .unwrap();
    let lease = s
        .borrow_spend(borrow("claude@shop", "gpu"), None)
        .unwrap()
        .lease
        .lease_id;
    let ends = race(dir.path(), |s| {
        s.release_spend("claude@shop", &lease, None)
            .map(|_| ())
            .map_err(|e| e.code().to_string())
    });
    assert_eq!(ends.iter().filter(|e| e.is_ok()).count(), 1, "{ends:?}");
    assert!(
        ends.iter()
            .filter_map(|e| e.as_ref().err())
            .all(|e| e == "conflict"),
        "{ends:?}"
    );
}

/// BUGS-FOUND.md #7: the digits aren't checked, only that `str::parse` takes them and that the
/// text is five long, so a sign gets through and is stored as written. PR #25 refuses it, with
/// its own tests. What holds on both sides: a time is either refused or kept exactly as given.
#[test]
fn a_summary_time_with_a_plus_sign_is_refused_or_kept_as_written() {
    let (s, _) = store();
    for odd in ["+9:00", "09:+5", "+1:+2"] {
        if let Ok(set) = s.set_summary_schedule(Some(odd), None) {
            assert_eq!(set.daily_at.as_deref(), Some(odd));
        }
    }
}
