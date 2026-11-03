//! Remote agents: invites (single use, expiry, revocation, non-widening), redemption,
//! scope isolation of invite tokens across every surface, presence and ephemeral expiry,
//! leaving, kicking, webhook scope, and upgrade from v10.

use another_core::*;

type Handle = std::sync::Arc<std::sync::atomic::AtomicI64>;
const HOUR: i64 = 3_600_000;

fn hub() -> (Store, Handle) {
    let (clock, h) = Clock::manual(1_795_219_200_000);
    let s = Store::open_in_memory(clock).unwrap();
    s.ensure_project("t", "/work/shop").unwrap();
    s.ensure_project("t", "/work/vault").unwrap();
    for (name, project) in [
        ("manager", None),
        ("vaulter", Some("/work/vault")),
        ("claude", Some("/work/shop")),
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
    for (lane, project, lead) in [
        ("api", "shop", "claude@shop"),
        ("keys", "vault", "vaulter@vault"),
    ] {
        s.create_lane(
            NewLane {
                agent: "manager".into(),
                name: lane.into(),
                project: project.into(),
                lead: lead.into(),
                charter: format!("{project} charter"),
                status: Some(LaneStatus::Working),
            },
            None,
        )
        .unwrap();
    }
    (s, h)
}

fn advance(h: &Handle, ms: i64) {
    h.fetch_add(ms, std::sync::atomic::Ordering::SeqCst);
}

fn invite(s: &Store, f: impl FnOnce(&mut NewAgentInvite)) -> CreatedInvite {
    let mut i = NewAgentInvite {
        agent: "cloud".into(),
        program: Program::Claude,
        project_globs: vec!["/work/shop".into()],
        lane: Some("api".into()),
        expires_in_seconds: None,
        max_uses: None,
        ephemeral: true,
        idle_hours: Some(2),
        token_ttl_seconds: None,
    };
    f(&mut i);
    s.create_agent_invite(i).unwrap()
}

fn join(s: &Store, code: &str) -> Result<Joined> {
    s.redeem_invite(
        code,
        JoinClient {
            hostname: Some("cloud-box".into()),
            version: Some("0.1.0".into()),
        },
    )
}

#[test]
fn invites_are_single_use_expiring_and_not_widened() {
    let (s, clock) = hub();
    let c = invite(&s, |_| {});
    assert!(c.code.starts_with("anj_"));
    assert_eq!(
        invite_id_of(&c.code).as_deref(),
        Some(c.invite.invite_id.as_str())
    );
    assert_eq!(c.invite.status, "active");
    let j = join(&s, &c.code).unwrap();
    assert_eq!(j.agent, "cloud@shop", "registered in the lane's project");
    assert_eq!(j.project_globs, vec!["/work/shop".to_string()]);
    assert_eq!(j.lane.as_deref(), Some("api"));
    let scope = s.verify_token(&j.token).unwrap();
    assert_eq!(scope.project_globs, vec!["/work/shop".to_string()]);
    assert_eq!(s.agent("cloud@shop").unwrap().program, "claude");

    // Reuse, wrong secret, garbage: all the same forbidden error.
    let reuse = join(&s, &c.code).unwrap_err();
    assert_eq!(reuse.code(), "forbidden");
    let forged = format!("anj_{}_{}", c.invite.invite_id, "0".repeat(48));
    let wrong = join(&s, &forged).unwrap_err();
    let junk = join(&s, "anj_nope").unwrap_err();
    assert_eq!(reuse.to_string(), wrong.to_string());
    assert_eq!(reuse.to_string(), junk.to_string());
    assert_eq!(s.agent_invites().unwrap()[0].status, "used");

    // Expired.
    let e = invite(&s, |i| {
        i.agent = "late".into();
        i.expires_in_seconds = Some(3600);
    });
    advance(&clock, HOUR + 1);
    assert_eq!(join(&s, &e.code).unwrap_err().code(), "forbidden");
    // Revoked.
    let r = invite(&s, |i| i.agent = "revoked".into());
    s.revoke_agent_invite(&r.invite.invite_id).unwrap();
    assert_eq!(join(&s, &r.code).unwrap_err().code(), "forbidden");
    // Multi-use invites issue one token per use.
    let m = invite(&s, |i| {
        i.agent = "pool".into();
        i.max_uses = Some(2);
        i.lane = None;
    });
    let a = join(&s, &m.code).unwrap();
    let b = join(&s, &m.code).unwrap();
    assert_ne!(a.token_id, b.token_id);
    assert_eq!(
        a.agent, "pool@shop",
        "globs matching one project put the agent there"
    );
    assert!(join(&s, &m.code).is_err());

    // Creation rules: globs required, lane must match the globs, sane limits.
    let bad = |f: Box<dyn FnOnce(&mut NewAgentInvite)>| {
        let mut i = NewAgentInvite {
            agent: "x".into(),
            program: Program::Other,
            project_globs: vec!["/work/shop".into()],
            lane: None,
            expires_in_seconds: None,
            max_uses: None,
            ephemeral: true,
            idle_hours: None,
            token_ttl_seconds: None,
        };
        f(&mut i);
        s.create_agent_invite(i).unwrap_err().code()
    };
    assert_eq!(bad(Box::new(|i| i.project_globs.clear())), "invalid");
    assert_eq!(bad(Box::new(|i| i.lane = Some("keys".into()))), "invalid");
    assert_eq!(
        bad(Box::new(|i| i.expires_in_seconds = Some(60 * 86_400))),
        "invalid"
    );
    assert_eq!(bad(Box::new(|i| i.max_uses = Some(0))), "invalid");

    // Invite events never reach tokens or peers.
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
    assert!(feed.items.iter().all(|i| !i.kind.starts_with("invite.")));
}

#[test]
fn invite_token_only_sees_its_project() {
    let (s, _) = hub();
    // Project B (vault) data of every kind.
    s.report_lane(
        LaneReportInput {
            agent: "vaulter@vault".into(),
            lane: "keys".into(),
            kind: ReportKind::Status,
            summary: "vault secret progress".into(),
            progress: Some(10),
            status: None,
            links: vec![],
            artifacts: vec![],
            outcome: None,
            pr_url: None,
        },
        None,
    )
    .unwrap();
    s.ask_decision(
        NewDecision {
            agent: "vaulter@vault".into(),
            question: "Rotate vault keys?".into(),
            options: vec![
                DecisionOption {
                    value: "yes".into(),
                    label: "Yes".into(),
                    description: None,
                },
                DecisionOption {
                    value: "no".into(),
                    label: "No".into(),
                    description: None,
                },
            ],
            ..Default::default()
        },
        None,
    )
    .unwrap();
    s.create_spend_account(
        NewSpendAccount {
            name: "vault-gpu".into(),
            kind: SpendKind::GpuPod,
            unit: "USD".into(),
            budget: Some(10.0),
            period: SpendPeriod::Monthly,
            reset_day: 1,
            cost_per_hour: None,
            requires_approval: false,
            approver: None,
            project: Some("vault".into()),
            description: None,
        },
        None,
    )
    .unwrap();
    s.record_spend(
        RecordSpend {
            agent: Some("vaulter@vault".into()),
            account: "vault-gpu".into(),
            amount: 9.0,
            ..Default::default()
        },
        None,
    )
    .unwrap();

    let j = join(&s, &invite(&s, |_| {}).code).unwrap();
    let scope = s.verify_token(&j.token).unwrap();
    assert_eq!(
        s.lanes(LaneFilter::default(), Some(&scope))
            .unwrap()
            .iter()
            .map(|l| l.name.as_str())
            .collect::<Vec<_>>(),
        vec!["api"]
    );
    assert_eq!(
        s.lane("keys", Some(&scope)).unwrap_err().code(),
        "not_found"
    );
    assert!(s
        .decisions(DecisionFilter::default(), Some(&scope))
        .unwrap()
        .is_empty());
    let spend = s.spend_report(SpendQuery::default(), Some(&scope)).unwrap();
    assert!(spend.accounts.iter().all(|a| a.account.name != "vault-gpu"));
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
    let text = serde_json::to_string(&feed).unwrap();
    for leak in ["vault", "keys", "Rotate", "vaulter"] {
        assert!(!text.contains(leak), "{leak}: {text}");
    }
    let summary = serde_json::to_string(&s.daily_summary(None, Some(&scope)).unwrap()).unwrap();
    assert!(!summary.contains("vault"), "{summary}");

    // A webhook subscribed with this token never carries vault events.
    let hook = s
        .create_webhook(
            NewWebhook {
                url: "https://hooks.example.test/x".into(),
                filters: vec![WebhookFilter::All],
                project_globs: None,
                description: None,
            },
            Some(&scope),
        )
        .unwrap();
    assert_eq!(
        hook.webhook.project_globs,
        Some(vec!["/work/shop".to_string()])
    );
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
    s.update_lane(
        LaneUpdate {
            agent: "manager".into(),
            lane: "api".into(),
            status: Some(LaneStatus::Blocked),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    s.queue_webhook_deliveries().unwrap();
    let out = s.claim_due_deliveries(50).unwrap();
    assert!(!out.is_empty());
    assert!(out
        .iter()
        .all(|d| !d.body.contains("vault") && !d.body.contains("keys")));
}

#[test]
fn kicked_agents_lose_access_immediately() {
    let (s, _) = hub();
    let j = join(&s, &invite(&s, |i| i.ephemeral = false).code).unwrap();
    let other = s
        .create_token("cloud@shop", &["/work/shop".into()], None, None)
        .unwrap();
    s.acquire_holds(AcquireHolds {
        agent: "cloud@shop".into(),
        project: "shop".into(),
        paths: vec!["src/x.rs".into()],
        ttl_seconds: None,
        exclusive: true,
        reason: None,
        lane: None,
    })
    .unwrap();
    assert!(s.verify_token(&j.token).is_ok());
    let d = s.kick_agent("cloud@shop", Some("misbehaving")).unwrap();
    assert_eq!(d.tokens_revoked, 2, "every token, not just the invite's");
    assert_eq!(d.holds_released, 1);
    assert_eq!(d.lanes, vec!["api"]);
    assert!(s.verify_token(&j.token).is_err());
    assert!(s.verify_token(&other.secret).is_err());
    assert!(s.holds(HoldFilter::default()).unwrap().is_empty());
    let kinds: Vec<String> = s.events(500).unwrap().into_iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&"agent.kicked".to_string()));
    assert!(kinds.contains(&"lane.agent_left".to_string()));
    assert_eq!(s.remote_agents().unwrap()[0].presence, Presence::Gone);
}

#[test]
fn ephemeral_agents_expire_and_leave() {
    let (s, clock) = hub();
    let j = join(&s, &invite(&s, |_| {}).code).unwrap();
    s.heartbeat("cloud@shop", false).unwrap();
    s.acquire_holds(AcquireHolds {
        agent: "cloud@shop".into(),
        project: "shop".into(),
        paths: vec!["src/y.rs".into()],
        ttl_seconds: Some(4 * 3600),
        exclusive: true,
        reason: None,
        lane: None,
    })
    .unwrap();
    assert_eq!(s.remote_agents().unwrap()[0].presence, Presence::Online);
    advance(&clock, 10 * 60_000);
    assert_eq!(s.remote_agents().unwrap()[0].presence, Presence::Idle);
    advance(&clock, 2 * HOUR);
    s.sweep().unwrap();
    let r = &s.remote_agents().unwrap()[0];
    assert_eq!(r.presence, Presence::Gone);
    assert_eq!(r.left_reason.as_deref(), Some("idle"));
    assert!(s.verify_token(&j.token).is_err());
    assert!(s.holds(HoldFilter::default()).unwrap().is_empty());
    let left = s
        .events(500)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "lane.agent_left")
        .unwrap();
    assert_eq!(left.payload["reason"], "idle");

    // Session end: an ephemeral agent goes at once; leave() does the same.
    let j2 = join(&s, &invite(&s, |i| i.agent = "short".into()).code).unwrap();
    s.heartbeat("short@shop", true).unwrap();
    assert!(s.verify_token(&j2.token).is_err());
    let j3 = join(&s, &invite(&s, |i| i.agent = "polite".into()).code).unwrap();
    let d = s.leave("polite@shop").unwrap();
    assert_eq!(d.reason, "left");
    assert!(s.verify_token(&j3.token).is_err());
    // A persistent agent's leave only ends its session; its token keeps working.
    let j4 = join(
        &s,
        &invite(&s, |i| {
            i.agent = "steady".into();
            i.ephemeral = false;
        })
        .code,
    )
    .unwrap();
    s.leave("steady@shop").unwrap();
    advance(&clock, 10 * HOUR);
    s.sweep().unwrap();
    assert!(s.verify_token(&j4.token).is_ok());
}

#[test]
fn upgrades_a_database_created_before_remote_agents() {
    let dir = tempfile::tempdir().unwrap();
    {
        let old = Store::open_at_schema(dir.path(), 10).unwrap();
        old.ensure_project("t", "/work/shop").unwrap();
    }
    let s = Store::open(dir.path()).unwrap();
    assert!(s.agent_invites().unwrap().is_empty());
    let c = s
        .create_agent_invite(NewAgentInvite {
            agent: "a".into(),
            program: Program::Codex,
            project_globs: vec!["shop".into()],
            lane: None,
            expires_in_seconds: None,
            max_uses: None,
            ephemeral: true,
            idle_hours: None,
            token_ttl_seconds: None,
        })
        .unwrap();
    assert_eq!(
        s.redeem_invite(&c.code, JoinClient::default())
            .unwrap()
            .agent,
        "a@shop"
    );
}
