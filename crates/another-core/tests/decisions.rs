//! Human decisions: asking, answering (mail in the thread, request closed, lane released),
//! validation, timeouts, cancellation, token scoping, webhook filters and upgrade from v7.

use another_core::*;

type Handle = std::sync::Arc<std::sync::atomic::AtomicI64>;

fn store() -> (Store, Handle) {
    let (clock, handle) = Clock::manual(1_700_000_000_000);
    let s = Store::open_in_memory(clock).unwrap();
    s.ensure_project("t", "/work/shop").unwrap();
    s.ensure_project("t", "/work/vault").unwrap();
    for (name, project) in [
        ("manager", None),
        ("claude", Some("/work/shop")),
        ("codex", Some("/work/vault")),
    ] {
        s.register_agent_trusted(RegisterAgent {
            name: name.into(),
            // The manager relays answers, which only manager bots (and people) may do.
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
    (s, handle)
}

fn advance(h: &Handle, ms: i64) {
    h.fetch_add(ms, std::sync::atomic::Ordering::SeqCst);
}

fn opts(pairs: &[(&str, &str)]) -> Vec<DecisionOption> {
    pairs
        .iter()
        .map(|(v, l)| DecisionOption {
            value: v.to_string(),
            label: l.to_string(),
            description: None,
        })
        .collect()
}

fn ask(s: &Store, agent: &str, f: impl FnOnce(&mut NewDecision)) -> Result<Decision> {
    let mut input = NewDecision {
        agent: agent.into(),
        question: "Ship the refactor behind a flag, or wait for review?".into(),
        options: opts(&[("ship", "Ship behind a flag"), ("wait", "Wait for review")]),
        context_links: vec!["https://example.test/pr/7".into()],
        ..Default::default()
    };
    f(&mut input);
    s.ask_decision(input, None)
}

fn answer(s: &Store, id: &str, value: Option<&str>, text: Option<&str>) -> Result<Decision> {
    s.answer_decision(
        AnswerDecision {
            decision: id.into(),
            value: value.map(Into::into),
            text: text.map(Into::into),
            answered_by: None,
            relayed_by: Some("manager".into()),
        },
        None,
    )
}

#[test]
fn ask_answer_mail_request_and_lane() {
    let (s, _) = store();
    s.create_lane(
        NewLane {
            agent: "manager".into(),
            name: "refactor".into(),
            project: "shop".into(),
            lead: "claude@shop".into(),
            charter: String::new(),
            status: Some(LaneStatus::Working),
        },
        None,
    )
    .unwrap();
    let d = ask(&s, "claude@shop", |d| d.lane = Some("refactor".into())).unwrap();
    assert_eq!(d.status, DecisionStatus::Open);
    assert_eq!(d.human, DEFAULT_DECIDER);
    assert_eq!(d.project, "shop", "from the lane");
    assert_eq!(d.lane.as_deref(), Some("refactor"));

    // The person is an agent with program human, and has the question in their inbox.
    let human = s.agent(DEFAULT_DECIDER).unwrap();
    assert_eq!(human.program, "human");
    let inbox = s.inbox(DEFAULT_DECIDER, InboxQuery::default()).unwrap();
    let q = &inbox.messages[0].message;
    assert!(
        q.subject.starts_with("[decision] claude@shop asks"),
        "{}",
        q.subject
    );
    assert!(
        !q.subject.contains("Ship"),
        "subjects carry no question text"
    );
    assert!(q.body.contains("Ship the refactor"));
    assert!(q.body.contains("1. Ship behind a flag [ship]"));
    assert!(q.body.contains("Link: https://example.test/pr/7"));
    assert_eq!(q.importance, Importance::High);

    // It is a request of kind decision; request commands refuse it.
    let req = s
        .requests(RequestFilter::default())
        .unwrap()
        .into_iter()
        .find(|r| r.request_id == d.request_id)
        .unwrap();
    assert_eq!(req.kind, "decision");
    let claim = s.update_request(RequestUpdate {
        agent: "manager".into(),
        request_id: d.request_id.clone(),
        action: RequestAction::Claim,
        note: None,
        due_in_seconds: None,
    });
    assert_eq!(claim.unwrap_err().code(), "conflict");

    // The lane shows needs_decision while it is open.
    assert_eq!(
        s.lane("refactor", None).unwrap().lane.status,
        LaneStatus::NeedsDecision
    );

    // Validation of answers.
    assert_eq!(
        answer(&s, &d.decision_id, Some("maybe"), None)
            .unwrap_err()
            .code(),
        "invalid"
    );
    assert_eq!(
        answer(&s, &d.decision_id, None, Some("why not both"))
            .unwrap_err()
            .code(),
        "invalid",
        "free text not allowed"
    );

    // Answer by number, relayed by the manager.
    let done = answer(&s, &d.decision_id, Some("2"), None).unwrap();
    assert_eq!(done.status, DecisionStatus::Answered);
    let a = done.answer.unwrap();
    assert_eq!(a.value.as_deref(), Some("wait"));
    assert_eq!(a.label.as_deref(), Some("Wait for review"));
    assert_eq!(a.answered_by, DEFAULT_DECIDER);
    assert_eq!(a.relayed_by.as_deref(), Some("manager"));

    // The asking agent has the answer, in the same thread, flagged high.
    let inbox = s.inbox("claude@shop", InboxQuery::default()).unwrap();
    let m = &inbox.messages[0].message;
    assert_eq!(m.thread_id, d.thread_id);
    assert_eq!(m.from, DEFAULT_DECIDER);
    assert!(
        m.subject.starts_with("[decision] answered: wait"),
        "{}",
        m.subject
    );
    assert!(m.body.contains("Value: wait"));
    assert!(m.body.contains("relayed by manager"));
    assert_eq!(m.importance, Importance::High);

    // Request closed, lane back to working, second answer refused.
    let req = s
        .requests(RequestFilter::default())
        .unwrap()
        .into_iter()
        .find(|r| r.request_id == d.request_id)
        .unwrap();
    assert_eq!(req.status, RequestStatus::Done);
    assert_eq!(
        s.lane("refactor", None).unwrap().lane.status,
        LaneStatus::Working
    );
    assert_eq!(
        answer(&s, &d.decision_id, Some("ship"), None)
            .unwrap_err()
            .code(),
        "conflict"
    );
    let kinds: Vec<String> = s.events(200).unwrap().into_iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&"decision.created".to_string()));
    assert!(kinds.contains(&"decision.answered".to_string()));
    assert!(
        !kinds.contains(&"request.created".to_string()),
        "one event per ask"
    );

    // Markdown for a phone.
    let md = decision_markdown(&s.decision(&d.decision_id, None).unwrap());
    assert!(md.contains("1. Ship behind a flag (`ship`)"));
    assert!(md.contains("**Answer:** Wait for review"));
    assert!(md.lines().count() < 15);
}

#[test]
fn validation_and_free_text() {
    let (s, _) = store();
    let bad = |f: Box<dyn FnOnce(&mut NewDecision)>| ask(&s, "claude@shop", f).unwrap_err().code();
    assert_eq!(bad(Box::new(|d| d.options.truncate(1))), "invalid");
    assert_eq!(
        bad(Box::new(|d| {
            d.options = opts(&[
                ("a", "A"),
                ("b", "B"),
                ("c", "C"),
                ("d", "D"),
                ("e", "E"),
                ("f", "F"),
                ("g", "G"),
            ])
        })),
        "invalid"
    );
    assert_eq!(
        bad(Box::new(|d| d.options = opts(&[("a", "A"), ("a", "B")]))),
        "invalid"
    );
    assert_eq!(
        bad(Box::new(|d| d.options = opts(&[("a b", "A"), ("c", "C")]))),
        "invalid"
    );
    assert_eq!(
        bad(Box::new(|d| d.on_timeout = DecisionTimeout::ChooseDefault)),
        "invalid",
        "needs a default"
    );
    assert_eq!(
        bad(Box::new(|d| {
            d.on_timeout = DecisionTimeout::ChooseDefault;
            d.default_value = Some("wait".into());
        })),
        "invalid",
        "needs a deadline"
    );
    assert_eq!(
        bad(Box::new(|d| d.default_value = Some("nope".into()))),
        "invalid"
    );
    assert_eq!(bad(Box::new(|d| d.question = " ".into())), "invalid");
    // A project-less agent must name a project.
    assert_eq!(ask(&s, "manager", |_| {}).unwrap_err().code(), "invalid");

    let d = ask(&s, "claude@shop", |d| {
        d.allow_free_text = true;
        d.human = Some("sam".into());
    })
    .unwrap();
    assert_eq!(d.human, "sam");
    let done = answer(&s, &d.decision_id, None, Some("Ship it, but tell QA")).unwrap();
    assert_eq!(done.answer.as_ref().unwrap().value, None);
    assert_eq!(
        done.answer.as_ref().unwrap().text.as_deref(),
        Some("Ship it, but tell QA")
    );
}

#[test]
fn timeouts_default_cancel_and_keep_open() {
    let (s, clock) = store();
    let now = s.now();
    let dflt = ask(&s, "claude@shop", |d| {
        d.deadline_at = Some(now + 3_600_000);
        d.on_timeout = DecisionTimeout::ChooseDefault;
        d.default_value = Some("wait".into());
    })
    .unwrap();
    let cancel = ask(&s, "claude@shop", |d| {
        d.deadline_at = Some(now + 3_600_000);
        d.on_timeout = DecisionTimeout::Cancel;
    })
    .unwrap();
    let keep = ask(&s, "claude@shop", |d| d.deadline_at = Some(now + 3_600_000)).unwrap();
    s.sweep().unwrap();
    assert_eq!(
        s.decisions(DecisionFilter::default(), None).unwrap().len(),
        3
    );

    advance(&clock, 3_600_000);
    s.sweep().unwrap();
    s.sweep().unwrap();
    let d = s.decision(&dflt.decision_id, None).unwrap();
    assert_eq!(d.status, DecisionStatus::Expired);
    let a = d.answer.unwrap();
    assert_eq!(a.value.as_deref(), Some("wait"));
    assert_eq!(a.answered_by, "timeout");
    let c = s.decision(&cancel.decision_id, None).unwrap();
    assert_eq!(c.status, DecisionStatus::Expired);
    assert!(c.answer.is_none());
    let k = s.decision(&keep.decision_id, None).unwrap();
    assert_eq!(k.status, DecisionStatus::Open);

    let events = s.events(500).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == "decision.overdue")
            .count(),
        1,
        "one reminder"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == "decision.expired")
            .count(),
        2
    );
    let inbox = s.inbox("claude@shop", InboxQuery::default()).unwrap();
    let subjects: Vec<&str> = inbox
        .messages
        .iter()
        .map(|m| m.message.subject.as_str())
        .collect();
    assert!(
        subjects
            .iter()
            .any(|s| s.starts_with("[decision] default applied: wait")),
        "{subjects:?}"
    );
    assert!(subjects
        .iter()
        .any(|s| s.starts_with("[decision] timed out, cancelled")));
    let human = s.inbox(DEFAULT_DECIDER, InboxQuery::default()).unwrap();
    assert!(human
        .messages
        .iter()
        .any(|m| m.message.subject.starts_with("[decision] overdue (")));

    // A keep_open decision can still be answered after its deadline.
    answer(&s, &keep.decision_id, Some("ship"), None).unwrap();
}

#[test]
fn cancel_only_by_the_asker() {
    let (s, _) = store();
    let d = ask(&s, "claude@shop", |_| {}).unwrap();
    assert_eq!(
        s.cancel_decision("manager", &d.decision_id, None, None)
            .unwrap_err()
            .code(),
        "forbidden"
    );
    let c = s
        .cancel_decision(
            "claude@shop",
            &d.decision_id,
            Some("solved it".into()),
            None,
        )
        .unwrap();
    assert_eq!(c.status, DecisionStatus::Cancelled);
    let human = s.inbox(DEFAULT_DECIDER, InboxQuery::default()).unwrap();
    assert!(human.messages[0]
        .message
        .subject
        .starts_with("[decision] withdrawn ("));
    assert_eq!(
        answer(&s, &d.decision_id, Some("ship"), None)
            .unwrap_err()
            .code(),
        "conflict"
    );
}

#[test]
fn token_scope_hides_other_projects_decisions() {
    let (s, _) = store();
    let shop = ask(&s, "claude@shop", |_| {}).unwrap();
    let vault = ask(&s, "codex@vault", |d| {
        d.question = "Rotate the vault keys now?".into()
    })
    .unwrap();
    let t = s
        .create_token("manager", &["/work/shop".into()], None, None)
        .unwrap();
    let scope = s.verify_token(&t.secret).unwrap();

    let visible = s
        .decisions(DecisionFilter::default(), Some(&scope))
        .unwrap();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].decision_id, shop.decision_id);
    // A hidden decision is exactly a missing one: show, answer and cancel.
    let hidden = s.decision(&vault.decision_id, Some(&scope)).unwrap_err();
    let missing = s.decision("NO-SUCH-DECISION", Some(&scope)).unwrap_err();
    assert_eq!((hidden.code(), missing.code()), ("not_found", "not_found"));
    assert_eq!(
        hidden.to_string().replace(&vault.decision_id, "X"),
        missing.to_string().replace("NO-SUCH-DECISION", "X")
    );
    let r = s.answer_decision(
        AnswerDecision {
            decision: vault.decision_id.clone(),
            value: Some("rotate".into()),
            relayed_by: Some("manager".into()),
            ..Default::default()
        },
        Some(&scope),
    );
    assert_eq!(r.unwrap_err().code(), "not_found");
    let r = s.cancel_decision("manager", &vault.decision_id, None, Some(&scope));
    assert_eq!(r.unwrap_err().code(), "not_found");
    let r = s.ask_decision(
        NewDecision {
            agent: "manager".into(),
            question: "sneak".into(),
            options: opts(&[("a", "A"), ("b", "B")]),
            project: Some("vault".into()),
            ..Default::default()
        },
        Some(&scope),
    );
    assert_eq!(
        r.unwrap_err().code(),
        "not_found",
        "out of scope looks like missing"
    );

    // Events: the vault decision never shows in the scoped feed.
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
    let text: String = feed
        .items
        .iter()
        .map(|i| i.summary.clone() + "\n")
        .collect();
    assert!(text.contains("asked for a decision"), "{text}");
    assert!(!text.contains("vault"), "{text}");
}

#[test]
fn webhook_filters_cover_decisions() {
    let (s, _) = store();
    let hook = s
        .create_webhook(
            NewWebhook {
                url: "https://hooks.example.test/x".into(),
                filters: vec![WebhookFilter::NeedsDecision, WebhookFilter::RequestAnswered],
                project_globs: Some(vec!["shop".into()]),
                description: None,
            },
            None,
        )
        .unwrap();
    let d = ask(&s, "claude@shop", |_| {}).unwrap();
    answer(&s, &d.decision_id, Some("ship"), None).unwrap();
    ask(&s, "codex@vault", |_| {}).unwrap();
    s.queue_webhook_deliveries().unwrap();
    let ds = s
        .webhook_deliveries(Some(&hook.webhook.webhook_id), None, 50)
        .unwrap();
    let mut kinds: Vec<(String, String)> = ds.into_iter().map(|d| (d.kind, d.filter)).collect();
    kinds.sort();
    assert_eq!(
        kinds,
        vec![
            (
                "decision.answered".to_string(),
                "request_answered".to_string()
            ),
            ("decision.created".to_string(), "needs_decision".to_string()),
        ]
    );
}

#[test]
fn upgrades_a_database_created_before_decisions() {
    let dir = tempfile::tempdir().unwrap();
    {
        let old = Store::open_at_schema(dir.path(), 7).unwrap();
        old.ensure_project("t", "/work/shop").unwrap();
        for n in ["a", "b"] {
            old.register_agent_trusted(RegisterAgent {
                name: n.into(),
                program: Program::Other,
                project: Some("/work/shop".into()),
                model: None,
                description: None,
            })
            .unwrap();
        }
        old.send_message(SendMessage {
            from: "a@shop".into(),
            to: vec!["b@shop".into()],
            subject: "old request".into(),
            body: String::new(),
            request: Some(NewRequest::default()),
            ..Default::default()
        })
        .unwrap();
    }
    let s = Store::open(dir.path()).unwrap();
    let reqs = s.requests(RequestFilter::default()).unwrap();
    assert_eq!(reqs[0].kind, "task");
    ask(&s, "a@shop", |_| {}).unwrap();
}

#[test]
fn the_asker_cannot_answer_its_own_decision() {
    let (s, _) = store();
    let d = ask(&s, "claude@shop", |_| {}).unwrap();
    // The asker relaying an answer "from owner": refused.
    let own = s.answer_decision(
        AnswerDecision {
            decision: d.decision_id.clone(),
            value: Some("ship".into()),
            answered_by: Some("owner".into()),
            relayed_by: Some("claude@shop".into()),
            ..Default::default()
        },
        None,
    );
    let e = own.unwrap_err();
    assert_eq!(e.code(), "forbidden");
    assert!(e.to_string().contains("its own decision"), "{e}");
    // Another coding agent can't relay either; only a manager bot or a person.
    let other = s.answer_decision(
        AnswerDecision {
            decision: d.decision_id.clone(),
            value: Some("ship".into()),
            relayed_by: Some("codex@vault".into()),
            ..Default::default()
        },
        None,
    );
    assert_eq!(other.unwrap_err().code(), "forbidden");
    assert_eq!(
        s.decision(&d.decision_id, None).unwrap().status,
        DecisionStatus::Open
    );
    answer(&s, &d.decision_id, Some("ship"), None).unwrap();
}

#[test]
fn lanes_and_projects_resolve_within_the_callers_scope() {
    let (s, _) = store();
    for (lane, project, lead) in [
        ("shop-lane", "shop", "claude@shop"),
        ("vault-lane", "vault", "codex@vault"),
    ] {
        s.create_lane(
            NewLane {
                agent: "manager".into(),
                name: lane.into(),
                project: project.into(),
                lead: lead.into(),
                charter: String::new(),
                status: None,
            },
            None,
        )
        .unwrap();
    }
    // Locally, a lane is looked up in the decision's project only.
    let cross = ask(&s, "claude@shop", |d| d.lane = Some("vault-lane".into())).unwrap_err();
    let missing = ask(&s, "claude@shop", |d| d.lane = Some("nope".into())).unwrap_err();
    assert_eq!(cross.code(), "not_found");
    assert_eq!(
        cross.to_string().replace("vault-lane", "X"),
        missing.to_string().replace("nope", "X")
    );
    let vault_lane_id = s.lane("vault-lane", None).unwrap().lane.lane_id;
    assert_eq!(
        ask(&s, "claude@shop", |d| d.lane = Some(vault_lane_id.clone()))
            .unwrap_err()
            .code(),
        "not_found",
        "by id too"
    );
    ask(&s, "claude@shop", |d| d.lane = Some("shop-lane".into())).unwrap();

    // A token: the project is scope-checked first, and hidden looks like missing.
    let t = s
        .create_token("manager", &["/work/shop".into()], None, None)
        .unwrap();
    let scope = s.verify_token(&t.secret).unwrap();
    let scoped_ask = |project: &str, lane: Option<&str>| {
        s.ask_decision(
            NewDecision {
                agent: "manager".into(),
                question: "q?".into(),
                options: opts(&[("a", "A"), ("b", "B")]),
                project: Some(project.into()),
                lane: lane.map(Into::into),
                ..Default::default()
            },
            Some(&scope),
        )
        .unwrap_err()
    };
    let hidden = scoped_ask("vault", Some("vault-lane"));
    let nowhere = scoped_ask("nowhere", Some("vault-lane"));
    assert_eq!(hidden.code(), "not_found");
    assert_eq!(
        hidden.to_string().replace("vault", "X"),
        nowhere.to_string().replace("nowhere", "X"),
        "the project check comes first"
    );
    let lane_hidden = scoped_ask("shop", Some("vault-lane"));
    let lane_missing = scoped_ask("shop", Some("nope"));
    assert_eq!(
        lane_hidden.to_string().replace("vault-lane", "X"),
        lane_missing.to_string().replace("nope", "X")
    );

    // decision_list filters: the same rules.
    let list = |project: Option<&str>, lane: Option<&str>| {
        s.decisions(
            DecisionFilter {
                project: project.map(Into::into),
                lane: lane.map(Into::into),
                ..Default::default()
            },
            Some(&scope),
        )
    };
    let p_hidden = list(Some("vault"), None).unwrap_err();
    let p_missing = list(Some("nowhere"), None).unwrap_err();
    assert_eq!(
        p_hidden.to_string().replace("vault", "X"),
        p_missing.to_string().replace("nowhere", "X")
    );
    let l_hidden = list(None, Some("vault-lane")).unwrap_err();
    let l_missing = list(None, Some("nope")).unwrap_err();
    assert_eq!(
        l_hidden.to_string().replace("vault-lane", "X"),
        l_missing.to_string().replace("nope", "X")
    );
    assert_eq!(list(Some("shop"), Some("shop-lane")).unwrap().len(), 1);
}

#[test]
fn decider_setting_system_agent_and_option_notes() {
    let (s, clock) = store();
    // The default decider is a stored node setting.
    assert_eq!(s.default_decider().unwrap(), DEFAULT_DECIDER);
    s.set_setting("decider", "sam").unwrap();
    assert_eq!(s.setting("decider").unwrap().value, "sam");
    let d = ask(&s, "claude@shop", |d| {
        d.deadline_at = Some(s.now() + 60_000);
        d.on_timeout = DecisionTimeout::Cancel;
    })
    .unwrap();
    assert_eq!(d.human, "sam");
    assert_eq!(
        s.set_setting("decider", "not ok!").unwrap_err().code(),
        "invalid"
    );
    assert_eq!(
        s.set_setting("colour", "blue").unwrap_err().code(),
        "invalid"
    );

    // Timeout and overdue mail comes from the reserved system agent, not the person.
    let keep = ask(&s, "claude@shop", |d| {
        d.deadline_at = Some(s.now() + 60_000)
    })
    .unwrap();
    advance(&clock, 61_000);
    s.sweep().unwrap();
    let inbox = s.inbox("claude@shop", InboxQuery::default()).unwrap();
    let expired = inbox
        .messages
        .iter()
        .find(|m| {
            m.message
                .subject
                .starts_with("[decision] timed out, cancelled")
        })
        .unwrap();
    assert_eq!(expired.message.from, SYSTEM_AGENT);
    assert_eq!(expired.message.thread_id, d.thread_id);
    let sam = s.inbox("sam", InboxQuery::default()).unwrap();
    let overdue = sam
        .messages
        .iter()
        .find(|m| m.message.subject.starts_with("[decision] overdue ("))
        .unwrap();
    assert_eq!(overdue.message.from, SYSTEM_AGENT);
    assert_eq!(overdue.message.thread_id, keep.thread_id);
    // The name is reserved.
    let r = s.register_agent_trusted(RegisterAgent {
        name: SYSTEM_AGENT.into(),
        program: Program::Other,
        project: None,
        model: None,
        description: None,
    });
    assert_eq!(r.unwrap_err().code(), "invalid");

    // A short note may go with an option even when free text is off; not alone, not long.
    let n = ask(&s, "claude@shop", |_| {}).unwrap();
    assert_eq!(
        answer(&s, &n.decision_id, None, Some("just text"))
            .unwrap_err()
            .code(),
        "invalid"
    );
    let long = "x".repeat(MAX_OPTION_NOTE + 1);
    assert_eq!(
        answer(&s, &n.decision_id, Some("ship"), Some(&long))
            .unwrap_err()
            .code(),
        "invalid"
    );
    let done = answer(&s, &n.decision_id, Some("ship"), Some("ship it, ping QA")).unwrap();
    assert_eq!(
        done.answer.unwrap().text.as_deref(),
        Some("ship it, ping QA")
    );
}

#[test]
fn relayer_roles_are_bound_to_the_registration() {
    let s = Store::open_in_memory(Clock::System).unwrap();
    s.ensure_project("t", "/work/shop").unwrap();
    for (name, program) in [("claude", Program::Claude), ("manager", Program::GrokBot)] {
        s.register_agent_trusted(RegisterAgent {
            name: name.into(),
            program,
            project: Some("/work/shop".into()),
            model: None,
            description: None,
        })
        .unwrap();
    }
    let reg = |name: &str, program: Program| {
        s.register_agent(RegisterAgent {
            name: name.into(),
            program,
            project: Some("/work/shop".into()),
            model: None,
            description: None,
        })
    };
    // A coding agent can't promote itself to manager (or person)...
    for role in [Program::GrokBot, Program::Human] {
        assert_eq!(reg("claude", role).unwrap_err().code(), "forbidden");
    }
    assert_eq!(s.agent("claude@shop").unwrap().program, "claude");
    // ...and a hook refreshing the manager as its client keeps the role.
    let m = reg("manager", Program::Codex).unwrap();
    assert_eq!(m.program, "grok_bot");
    // So it still can't relay answers to someone else's decision.
    let d = s
        .ask_decision(
            NewDecision {
                agent: "manager@shop".into(),
                question: "Ship?".into(),
                options: opts(&[("yes", "Yes"), ("no", "No")]),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    let r = s.answer_decision(
        AnswerDecision {
            decision: d.decision_id.clone(),
            value: Some("yes".into()),
            relayed_by: Some("claude@shop".into()),
            ..Default::default()
        },
        None,
    );
    assert_eq!(r.unwrap_err().code(), "forbidden");
    // The person sets roles on the machine.
    s.register_agent_trusted(RegisterAgent {
        name: "claude".into(),
        program: Program::GrokBot,
        project: Some("/work/shop".into()),
        model: None,
        description: None,
    })
    .unwrap();
    assert_eq!(s.agent("claude@shop").unwrap().program, "grok_bot");
}

#[test]
fn decision_events_carry_no_question_or_option_text() {
    let s = Store::open_in_memory(Clock::System).unwrap();
    s.ensure_project("t", "/work/shop").unwrap();
    s.register_agent_trusted(RegisterAgent {
        name: "claude".into(),
        program: Program::Claude,
        project: Some("/work/shop".into()),
        model: None,
        description: None,
    })
    .unwrap();
    s.ask_decision(
        NewDecision {
            agent: "claude@shop".into(),
            question: "Rename the billing tables tonight?".into(),
            options: vec![
                DecisionOption {
                    value: "yes".into(),
                    label: "Go ahead with the rename".into(),
                    description: Some("drops the old views".into()),
                },
                DecisionOption {
                    value: "no".into(),
                    label: "Wait".into(),
                    description: None,
                },
            ],
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let ev = s
        .events(100)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "decision.created")
        .unwrap();
    let text = ev.payload.to_string();
    for leak in ["billing", "rename", "old views", "Wait"] {
        assert!(!text.contains(leak), "{leak} in {text}");
    }
    let feed = s
        .activity(ActivityQuery {
            limit: 100,
            ..Default::default()
        })
        .unwrap();
    assert!(!serde_json::to_string(&feed).unwrap().contains("billing"));
    // The question is still in the decision and its mail.
    let ds = s.decisions(DecisionFilter::default(), None).unwrap();
    assert_eq!(ds[0].question, "Rename the billing tables tonight?");
}

#[test]
fn the_cancelled_event_flags_a_reason_without_carrying_its_text() {
    let (s, _) = store();
    let cancelled = |id: &str| {
        s.events(100)
            .unwrap()
            .into_iter()
            .find(|e| e.kind == "decision.cancelled" && e.payload["decision_id"] == id)
            .expect("decision.cancelled")
    };

    let with_reason = ask(&s, "claude@shop", |_| {}).unwrap();
    s.cancel_decision(
        "claude@shop",
        &with_reason.decision_id,
        Some("the vendor contract fell through".into()),
        None,
    )
    .unwrap();
    let ev = cancelled(&with_reason.decision_id);
    assert_eq!(ev.payload["has_reason"], true);
    assert!(ev.payload.get("reason").is_none(), "{}", ev.payload);
    assert!(
        !ev.payload.to_string().contains("vendor contract"),
        "{}",
        ev.payload
    );
    // The reason still reaches the person, in the mail.
    let human = s.inbox(DEFAULT_DECIDER, InboxQuery::default()).unwrap();
    assert!(human
        .messages
        .iter()
        .any(|m| m.message.body.contains("the vendor contract fell through")));

    let without_reason = ask(&s, "claude@shop", |_| {}).unwrap();
    s.cancel_decision("claude@shop", &without_reason.decision_id, None, None)
        .unwrap();
    let ev = cancelled(&without_reason.decision_id);
    assert_eq!(ev.payload["has_reason"], false);
    assert!(ev.payload.get("reason").is_none(), "{}", ev.payload);
}

#[test]
fn round4_roles_people_and_exact_options() {
    let s = Store::open_in_memory(Clock::System).unwrap();
    s.ensure_project("t", "/work/shop").unwrap();
    let reg = |name: &str, program: Program, trusted: bool| {
        let input = RegisterAgent {
            name: name.into(),
            program,
            project: None,
            model: None,
            description: None,
        };
        if trusted {
            s.register_agent_trusted(input)
        } else {
            s.register_agent(input)
        }
    };
    // A brand-new untrusted registration can't pick a privileged role either.
    for role in [Program::GrokBot, Program::Human] {
        assert_eq!(reg("fresh", role, false).unwrap_err().code(), "forbidden");
    }
    assert!(s.agent("fresh").is_err());
    reg("claude", Program::Claude, false).unwrap();
    s.register_agent_trusted(RegisterAgent {
        name: "claude".into(),
        program: Program::Claude,
        project: Some("/work/shop".into()),
        model: None,
        description: None,
    })
    .unwrap();
    // A coding agent can't be the person a decision goes to.
    let ask = |human: &str, options: Vec<DecisionOption>| {
        s.ask_decision(
            NewDecision {
                agent: "claude@shop".into(),
                question: "Which?".into(),
                options,
                human: Some(human.into()),
                ..Default::default()
            },
            None,
        )
    };
    let e = ask("claude", opts(&[("yes", "Yes"), ("no", "No")])).unwrap_err();
    assert_eq!(e.code(), "invalid");
    assert!(e.to_string().contains("not a person"), "{e}");
    // Exact option values win over case-insensitive ones.
    reg("sam", Program::Human, true).unwrap();
    let d = ask("sam", opts(&[("A", "Upper"), ("a", "Lower")])).unwrap();
    let answered = s
        .answer_decision(
            AnswerDecision {
                decision: d.decision_id.clone(),
                value: Some("a".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    assert_eq!(answered.answer.unwrap().value.as_deref(), Some("a"));
}

#[test]
fn on_demand_people_cannot_relay_until_confirmed() {
    let s = Store::open_in_memory(Clock::System).unwrap();
    s.ensure_project("t", "/work/shop").unwrap();
    s.register_agent_trusted(RegisterAgent {
        name: "claude".into(),
        program: Program::Claude,
        project: Some("/work/shop".into()),
        model: None,
        description: None,
    })
    .unwrap();
    // A first-time untrusted registration can't pick the role either.
    let e = s
        .register_agent(RegisterAgent {
            name: "minted".into(),
            program: Program::Human,
            project: None,
            model: None,
            description: None,
        })
        .unwrap_err();
    assert_eq!(e.code(), "forbidden");
    // The agent asks an unused name: that person is created on demand as the addressee...
    let d = s
        .ask_decision(
            NewDecision {
                agent: "claude@shop".into(),
                question: "Merge my own PR?".into(),
                options: opts(&[("yes", "Yes"), ("no", "No")]),
                human: Some("sockpuppet".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    assert_eq!(s.agent("sockpuppet").unwrap().program, "human");
    // ...but can't be used to relay the answer to the agent's own question.
    let answer = |by: &str| {
        s.answer_decision(
            AnswerDecision {
                decision: d.decision_id.clone(),
                value: Some("yes".into()),
                relayed_by: Some(by.into()),
                ..Default::default()
            },
            None,
        )
    };
    let e = answer("sockpuppet").unwrap_err();
    assert_eq!(e.code(), "forbidden");
    assert!(e.to_string().contains("not a confirmed human"), "{e}");
    // Once the person on the machine confirms the role, it can.
    s.register_agent_trusted(RegisterAgent {
        name: "sockpuppet".into(),
        program: Program::Human,
        project: None,
        model: None,
        description: None,
    })
    .unwrap();
    answer("sockpuppet").unwrap();
}

#[test]
fn cancelling_before_a_choose_default_deadline_is_allowed() {
    let (s, _) = store();
    let now = s.now();
    let d = ask(&s, "claude@shop", |d| {
        d.deadline_at = Some(now + 3_600_000);
        d.on_timeout = DecisionTimeout::ChooseDefault;
        d.default_value = Some("wait".into());
    })
    .unwrap();
    let cancelled = s
        .cancel_decision("claude@shop", &d.decision_id, None, None)
        .unwrap();
    assert_eq!(cancelled.status, DecisionStatus::Cancelled);
    assert!(cancelled.answer.is_none());
    assert_eq!(
        s.request(&d.request_id).unwrap().status,
        RequestStatus::Cancelled
    );
}

#[test]
fn a_bare_lane_name_in_two_projects_is_ambiguous_not_unknown() {
    let (s, _) = store();
    for (project, lead) in [("shop", "claude@shop"), ("vault", "codex@vault")] {
        s.create_lane(
            NewLane {
                agent: "manager".into(),
                name: "api".into(),
                project: project.into(),
                lead: lead.into(),
                charter: String::new(),
                status: Some(LaneStatus::Working),
            },
            None,
        )
        .unwrap();
    }
    let e = s
        .decisions(
            DecisionFilter {
                lane: Some("api".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap_err();
    assert_eq!(e.code(), "ambiguous", "{e}");
    let unknown = s
        .decisions(
            DecisionFilter {
                lane: Some("nope".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap_err();
    assert_eq!(unknown.code(), "not_found", "{unknown}");
}

#[test]
fn cancelling_after_a_choose_default_deadline_keeps_the_default() {
    let (s, clock) = store();
    let now = s.now();
    let d = ask(&s, "claude@shop", |d| {
        d.deadline_at = Some(now + 3_600_000);
        d.on_timeout = DecisionTimeout::ChooseDefault;
        d.default_value = Some("wait".into());
    })
    .unwrap();
    // The deadline passes with no sweep in between (no server running).
    advance(&clock, 3_600_001);
    let e = s
        .cancel_decision("claude@shop", &d.decision_id, None, None)
        .unwrap_err();
    assert_eq!(e.code(), "conflict", "{e}");
    assert!(
        s.events(100)
            .unwrap()
            .iter()
            .any(|e| { e.kind == "decision.expired" && e.payload["decision_id"] == d.decision_id }),
        "cancellation must commit the timeout before returning, without another sweep"
    );
    let request = s.request(&d.request_id).unwrap();
    assert_eq!(request.status, RequestStatus::Done);
    assert!(request.closed_at.is_some());
    let thread = s.thread(&d.thread_id).unwrap();
    let automatic = thread
        .messages
        .iter()
        .filter(|m| m.subject.starts_with("[decision] default applied: wait"))
        .collect::<Vec<_>>();
    assert_eq!(automatic.len(), 1);
    assert_eq!(automatic[0].from, SYSTEM_AGENT);
    assert_eq!(automatic[0].to, ["claude@shop"]);
    let after = s.decision(&d.decision_id, None).unwrap();
    assert_eq!(after.status, DecisionStatus::Expired);
    assert_eq!(after.answer.unwrap().value.as_deref(), Some("wait"));
}

#[test]
fn decision_requests_are_scoped_by_the_decisions_project() {
    let (s, _) = store();
    // A shop agent asks a decision that belongs to vault.
    ask(&s, "claude@shop", |d| d.project = Some("vault".into())).unwrap();
    let scope_for = |glob: &str| {
        let t = s
            .create_token("manager", &[glob.into()], None, None)
            .unwrap();
        s.verify_token(&t.secret).unwrap()
    };
    let open = |glob: &str| {
        s.activity_summary_scoped(Some(Since::Time(0)), Some(&scope_for(glob)))
            .unwrap()
            .requests
            .open
    };
    assert_eq!(
        open("/work/shop"),
        0,
        "not shop's request, though a shop agent asked"
    );
    // From the daily summary on (D42), request counts are task requests only and decisions
    // are reported on their own: vault's token sees this one there, shop's doesn't.
    assert_eq!(
        open("/work/vault"),
        0,
        "decisions aren't counted as requests"
    );
    let decisions = |glob: &str| {
        s.daily_summary(None, Some(&scope_for(glob)))
            .unwrap()
            .decisions
            .open_total
    };
    assert_eq!((decisions("/work/shop"), decisions("/work/vault")), (0, 1));
}
