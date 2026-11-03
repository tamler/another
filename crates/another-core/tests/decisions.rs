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
        s.register_agent(RegisterAgent {
            name: name.into(),
            program: Program::Claude,
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
    assert!(q.subject.starts_with("[decision] Ship the refactor"));
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
    assert!(m.subject.starts_with("[decision] Wait for review"));
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
    assert!(subjects
        .iter()
        .any(|s| s.starts_with("[decision] default applied: Wait for review")));
    assert!(subjects
        .iter()
        .any(|s| s.starts_with("[decision] cancelled:")));
    let human = s.inbox(DEFAULT_DECIDER, InboxQuery::default()).unwrap();
    assert!(human
        .messages
        .iter()
        .any(|m| m.message.subject.starts_with("[decision] overdue:")));

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
        .starts_with("[decision] withdrawn:"));
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
    assert_eq!(
        s.decision(&vault.decision_id, Some(&scope))
            .unwrap_err()
            .code(),
        "forbidden"
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
    assert_eq!(r.unwrap_err().code(), "forbidden");
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
    assert_eq!(r.unwrap_err().code(), "forbidden");

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
    assert!(text.contains("Ship the refactor"), "{text}");
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
            old.register_agent(RegisterAgent {
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
