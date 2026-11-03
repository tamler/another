//! Lanes: lifecycle, structured reports delivered as mail, links, scoping, and upgrade
//! from a database created before lanes existed.

use another_core::*;

fn store() -> Store {
    let s = Store::open_in_memory(Clock::System).unwrap();
    s.ensure_project("t", "/work/shop").unwrap();
    s.ensure_project("t", "/work/vault").unwrap();
    for (name, program) in [
        ("manager", Program::GrokBot),
        ("claude", Program::Claude),
        ("codex", Program::Codex),
    ] {
        s.register_agent(RegisterAgent {
            name: name.into(),
            program,
            project: None,
            model: None,
            description: None,
        })
        .unwrap();
    }
    s
}

fn lane(s: &Store, name: &str, project: &str) -> LaneDetail {
    s.create_lane(
        NewLane {
            agent: "manager".into(),
            name: name.into(),
            project: project.into(),
            lead: "claude".into(),
            charter: "Ship the API refactor.\nKeep the public routes stable.".into(),
            status: None,
        },
        None,
    )
    .unwrap()
}

fn report(
    s: &Store,
    agent: &str,
    lane: &str,
    kind: ReportKind,
    summary: &str,
) -> Result<LaneReport> {
    s.report_lane(
        LaneReportInput {
            agent: agent.into(),
            lane: lane.into(),
            kind,
            summary: summary.into(),
            progress: None,
            status: None,
            links: vec![],
            artifacts: vec![],
            outcome: None,
            pr_url: None,
        },
        None,
    )
}

#[test]
fn lifecycle_reports_and_mail() {
    let s = store();
    let d = lane(&s, "API-Refactor", "shop");
    assert_eq!(d.lane.name, "api-refactor");
    assert_eq!(d.lane.status, LaneStatus::Planned);
    assert_eq!(d.lane.lead, "claude");
    assert_eq!(d.lane.created_by, "manager");
    assert_eq!(d.lane.project, "shop");

    // Names are unique among open lanes.
    let dup = s.create_lane(
        NewLane {
            agent: "manager".into(),
            name: "api-refactor".into(),
            project: "shop".into(),
            lead: "codex".into(),
            charter: String::new(),
            status: None,
        },
        None,
    );
    assert_eq!(dup.unwrap_err().code(), "conflict");

    // Status update by the manager, then a structured report from the lead.
    s.update_lane(
        LaneUpdate {
            agent: "manager".into(),
            lane: "api-refactor".into(),
            status: Some(LaneStatus::Working),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let r = s
        .report_lane(
            LaneReportInput {
                agent: "claude".into(),
                lane: "api-refactor".into(),
                kind: ReportKind::Status,
                summary: "Routes split into modules\nTests next.".into(),
                progress: Some(40),
                status: Some(LaneStatus::Blocked),
                links: vec!["https://example.test/ci/1".into()],
                artifacts: vec!["docs/api.md".into()],
                outcome: None,
                pr_url: None,
            },
            None,
        )
        .unwrap();
    assert_eq!(r.progress, Some(40));
    assert_eq!(r.status, Some(LaneStatus::Blocked));
    let msg = r.message_id.clone().expect("mailed to the creator");

    // The manager got it in its inbox, in the lane thread, flagged high because blocked.
    let inbox = s.inbox("manager", InboxQuery::default()).unwrap();
    let m = &inbox.messages[0].message;
    assert_eq!(m.message_id, msg);
    assert_eq!(
        m.subject,
        "[lane api-refactor] blocked: Routes split into modules"
    );
    assert!(m.body.contains("Progress: 40%"));
    assert!(m.body.contains("Artifact: docs/api.md"));
    assert_eq!(m.importance, Importance::High);
    let shown = s.lane("api-refactor", None).unwrap();
    assert_eq!(shown.lane.status, LaneStatus::Blocked);
    assert_eq!(shown.lane.thread_id.as_deref(), Some(m.thread_id.as_str()));
    assert_eq!(shown.lane.reports, 1);

    // A report from the manager goes to the lead, in the same thread.
    let back = report(
        &s,
        "manager",
        "api-refactor",
        ReportKind::Status,
        "Unblocked: use v2 schema",
    )
    .unwrap();
    let to_lead = s.message(back.message_id.as_deref().unwrap()).unwrap();
    assert_eq!(to_lead.to, vec!["claude"]);
    assert_eq!(to_lead.thread_id, m.thread_id);

    // Done report: outcome + PR, lane closed, name free again.
    let done = s
        .report_lane(
            LaneReportInput {
                agent: "claude".into(),
                lane: "api-refactor".into(),
                kind: ReportKind::Done,
                summary: "Merged-ready".into(),
                progress: Some(100),
                status: None,
                links: vec![],
                artifacts: vec![],
                outcome: Some("Refactor complete; routes unchanged".into()),
                pr_url: Some("https://github.com/example/repo/pull/7".into()),
            },
            None,
        )
        .unwrap();
    assert_eq!(done.kind, ReportKind::Done);
    let closed = s.lane("api-refactor", None).unwrap();
    assert_eq!(closed.lane.status, LaneStatus::Done);
    assert!(closed.lane.closed_at.is_some());
    assert_eq!(
        closed.lane.pr_url.as_deref(),
        Some("https://github.com/example/repo/pull/7")
    );
    assert_eq!(closed.reports[0].kind, ReportKind::Done);
    assert_eq!(
        report(&s, "claude", "api-refactor", ReportKind::Status, "late")
            .unwrap_err()
            .code(),
        "conflict"
    );
    // Closed lanes are hidden from the default list.
    assert!(s.lanes(LaneFilter::default(), None).unwrap().is_empty());
    let all = LaneFilter {
        include_closed: true,
        ..Default::default()
    };
    assert_eq!(s.lanes(all, None).unwrap().len(), 1);
    lane(&s, "api-refactor", "shop");

    // Markdown export mirrors the old STATUS file.
    let md = lane_markdown(&closed);
    assert!(md.starts_with("# Lane api-refactor: done"));
    assert!(md.contains("| PR | https://github.com/example/repo/pull/7 |"));
    assert!(md.contains("## Charter"));

    let kinds: Vec<String> = s.events(500).unwrap().into_iter().map(|e| e.kind).collect();
    for k in [
        "lane.created",
        "lane.status_changed",
        "lane.reported",
        "lane.closed",
    ] {
        assert!(kinds.contains(&k.to_string()), "{k}");
    }
    let feed = s
        .activity(ActivityQuery {
            kinds: vec![ActivityKind::Lane],
            ..Default::default()
        })
        .unwrap();
    assert!(feed
        .items
        .iter()
        .any(|i| i.summary.contains("lane api-refactor: working -> blocked")));
}

#[test]
fn validation_links_and_close() {
    let s = store();
    lane(&s, "docs", "shop");
    assert_eq!(
        report(&s, "claude", "docs", ReportKind::Status, " ")
            .unwrap_err()
            .code(),
        "invalid"
    );
    let bad = s.report_lane(
        LaneReportInput {
            agent: "claude".into(),
            lane: "docs".into(),
            kind: ReportKind::Done,
            summary: "x".into(),
            progress: None,
            status: None,
            links: vec![],
            artifacts: vec![],
            outcome: None,
            pr_url: None,
        },
        None,
    );
    assert_eq!(bad.unwrap_err().code(), "invalid", "done needs an outcome");

    // Link a thread and a request; unknown ids are refused.
    let m = s
        .send_message(SendMessage {
            from: "manager".into(),
            to: vec!["claude".into()],
            subject: "Please".into(),
            body: String::new(),
            request: Some(NewRequest::default()),
            ..Default::default()
        })
        .unwrap();
    let d = s
        .update_lane(
            LaneUpdate {
                agent: "manager".into(),
                lane: "docs".into(),
                links: vec![
                    (LaneLinkKind::Thread, m.thread_id.clone()),
                    (LaneLinkKind::Request, m.request_id.clone().unwrap()),
                ],
                ..Default::default()
            },
            None,
        )
        .unwrap();
    assert_eq!(d.links.len(), 2);
    let missing = s.update_lane(
        LaneUpdate {
            agent: "manager".into(),
            lane: "docs".into(),
            links: vec![(LaneLinkKind::Hold, "nope".into())],
            ..Default::default()
        },
        None,
    );
    assert_eq!(missing.unwrap_err().code(), "not_found");

    let abandoned = s
        .close_lane("manager", "docs", true, Some("superseded".into()), None)
        .unwrap();
    assert_eq!(abandoned.lane.status, LaneStatus::Abandoned);
    assert_eq!(abandoned.lane.outcome.as_deref(), Some("superseded"));
    assert_eq!(
        s.close_lane("manager", "docs", false, None, None)
            .unwrap_err()
            .code(),
        "conflict"
    );
}

#[test]
fn token_scope_hides_other_projects_lanes() {
    let s = store();
    lane(&s, "shop-work", "shop");
    lane(&s, "vault-work", "vault");
    let t = s
        .create_token("manager", &["/work/shop".into()], None, None)
        .unwrap();
    let scope = s.verify_token(&t.secret).unwrap();

    let visible: Vec<String> = s
        .lanes(LaneFilter::default(), Some(&scope))
        .unwrap()
        .into_iter()
        .map(|l| l.name)
        .collect();
    assert_eq!(visible, vec!["shop-work"]);
    assert_eq!(
        s.lane("vault-work", Some(&scope)).unwrap_err().code(),
        "forbidden"
    );
    assert!(s.lane("shop-work", Some(&scope)).is_ok());
    let r = s.report_lane(
        LaneReportInput {
            agent: "manager".into(),
            lane: "vault-work".into(),
            kind: ReportKind::Status,
            summary: "peek".into(),
            progress: None,
            status: None,
            links: vec![],
            artifacts: vec![],
            outcome: None,
            pr_url: None,
        },
        Some(&scope),
    );
    assert_eq!(r.unwrap_err().code(), "forbidden");
    let create = s.create_lane(
        NewLane {
            agent: "manager".into(),
            name: "sneak".into(),
            project: "vault".into(),
            lead: "claude".into(),
            charter: String::new(),
            status: None,
        },
        Some(&scope),
    );
    assert_eq!(create.unwrap_err().code(), "forbidden");
    assert_eq!(
        s.close_lane("manager", "vault-work", true, None, Some(&scope))
            .unwrap_err()
            .code(),
        "forbidden"
    );
}

#[test]
fn upgrades_a_database_created_before_lanes() {
    let dir = tempfile::tempdir().unwrap();
    {
        let old = Store::open_at_schema(dir.path(), 5).unwrap();
        old.ensure_project("t", "/work/shop").unwrap();
        for n in ["manager", "claude"] {
            old.register_agent(RegisterAgent {
                name: n.into(),
                program: Program::Other,
                project: None,
                model: None,
                description: None,
            })
            .unwrap();
        }
        old.send_message(SendMessage {
            from: "manager".into(),
            to: vec!["claude".into()],
            subject: "before the upgrade".into(),
            body: "kept".into(),
            ..Default::default()
        })
        .unwrap();
    }
    let s = Store::open(dir.path()).unwrap();
    let inbox = s.inbox("claude", InboxQuery::default()).unwrap();
    assert_eq!(inbox.messages[0].message.subject, "before the upgrade");
    let d = lane(&s, "after", "shop");
    assert_eq!(d.lane.status, LaneStatus::Planned);
    assert!(Store::schema_version() >= 6);
}
