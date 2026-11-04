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
        s.register_agent_trusted(RegisterAgent {
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
    assert_eq!(j.lane.as_deref(), Some("api@shop"));
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

#[test]
fn heartbeats_and_leaving_only_touch_the_tokens_projects() {
    let (s, _) = hub();
    // A persistent remote agent for shop, which (locally) also holds a vault file.
    let j = join(
        &s,
        &invite(&s, |i| {
            i.agent = "both".into();
            i.ephemeral = false;
        })
        .code,
    )
    .unwrap();
    let scope = s.verify_token(&j.token).unwrap();
    let hold = |project: &str, path: &str| {
        s.acquire_holds(AcquireHolds {
            agent: "both@shop".into(),
            project: project.into(),
            paths: vec![path.into()],
            ttl_seconds: Some(3600),
            exclusive: true,
            reason: None,
            lane: None,
        })
        .unwrap()
    };
    hold("shop", "src/a.rs");
    hold("vault", "keys/b.txt");
    let held = |project: &str| {
        s.holds(HoldFilter::default())
            .unwrap()
            .into_iter()
            .filter(|h| h.project == project)
            .count()
    };

    // A heartbeat through the token renews only shop's hold.
    let beat = s
        .session_scoped(
            "both@shop",
            SessionPhase::Beat,
            SessionInfo::default(),
            Some(&scope),
        )
        .unwrap();
    assert_eq!(beat.renewed_holds.len(), 1);
    assert_eq!(beat.renewed_holds[0].project, "shop");
    // Ending the session through the token releases shop's hold and keeps vault's.
    let end = s
        .session_scoped(
            "both@shop",
            SessionPhase::End,
            SessionInfo::default(),
            Some(&scope),
        )
        .unwrap();
    assert_eq!(end.released_holds.len(), 1);
    assert_eq!((held("shop"), held("vault")), (0, 1));

    // Leaving (ephemeral) and idle expiry are limited to the invite's projects too.
    let e = join(&s, &invite(&s, |i| i.agent = "eph".into()).code).unwrap();
    let escope = s.verify_token(&e.token).unwrap();
    for (project, path) in [("shop", "src/c.rs"), ("vault", "keys/d.txt")] {
        s.acquire_holds(AcquireHolds {
            agent: "eph@shop".into(),
            project: project.into(),
            paths: vec![path.into()],
            ttl_seconds: Some(3600),
            exclusive: true,
            reason: None,
            lane: None,
        })
        .unwrap();
    }
    let d = s.leave_scoped("eph@shop", Some(&escope)).unwrap();
    assert_eq!(d.holds_released, 1);
    let vault_holders: Vec<String> = s
        .holds(HoldFilter::default())
        .unwrap()
        .into_iter()
        .filter(|h| h.project == "vault")
        .map(|h| h.agent)
        .collect();
    assert!(
        vault_holders.contains(&"eph@shop".to_string()),
        "{vault_holders:?}"
    );
}

#[test]
fn invites_never_take_over_a_registered_name() {
    let (s, _) = hub();
    // claude@shop is a local agent: no invite may be made for that name.
    let e = s
        .create_agent_invite(NewAgentInvite {
            agent: "claude".into(),
            program: Program::Claude,
            project_globs: vec!["/work/shop".into()],
            lane: None,
            expires_in_seconds: None,
            max_uses: None,
            ephemeral: true,
            idle_hours: None,
            token_ttl_seconds: None,
        })
        .unwrap_err();
    assert_eq!(e.code(), "conflict");
    // A name registered after the invite was made: redeeming it fails like any bad code,
    // and the local agent gets no new token.
    let inv = invite(&s, |i| i.agent = "late".into());
    s.register_agent_trusted(RegisterAgent {
        name: "late".into(),
        program: Program::Codex,
        project: Some("/work/shop".into()),
        model: None,
        description: None,
    })
    .unwrap();
    let e = join(&s, &inv.code).unwrap_err();
    assert_eq!(e.code(), "forbidden");
    assert_eq!(e.to_string(), "invalid or expired invite");
    assert!(s.tokens().unwrap().iter().all(|t| t.agent != "late@shop"));
    // A multi-use invite's later uses refresh its own agent.
    let multi = invite(&s, |i| {
        i.agent = "pool".into();
        i.max_uses = Some(2);
    });
    join(&s, &multi.code).unwrap();
    join(&s, &multi.code).unwrap();
    // A remote agent that has gone (token revoked) can be invited again.
    s.leave("pool@shop").unwrap();
    invite(&s, |i| i.agent = "pool".into());
}

#[test]
fn a_departing_lead_hands_the_lane_to_the_longest_active_member() {
    let (s, clock) = hub();
    join(&s, &invite(&s, |i| i.agent = "first".into()).code).unwrap();
    advance(&clock, 60_000);
    join(&s, &invite(&s, |i| i.agent = "second".into()).code).unwrap();
    s.update_lane(
        LaneUpdate {
            agent: "manager".into(),
            lane: "api@shop".into(),
            lead: Some("first@shop".into()),
            ..Default::default()
        },
        None,
    )
    .unwrap();

    // The lead leaves: the member who joined next takes over, and is told.
    s.leave("first@shop").unwrap();
    let lane = s.lane("api@shop", None).unwrap().lane;
    assert_eq!(lane.lead, "second@shop");
    assert_eq!(lane.status, LaneStatus::Working);
    assert!(lane.leaderless_since.is_none());
    let inbox = s.inbox("second@shop", InboxQuery::default()).unwrap();
    let m = &inbox.messages[0].message;
    assert!(
        m.subject.contains("second@shop is now the lead"),
        "{}",
        m.subject
    );
    assert_eq!(m.from, "another");
    let creator = s.inbox("manager", InboxQuery::default()).unwrap();
    assert!(creator
        .messages
        .iter()
        .any(|m| m.message.subject.contains("is now the lead")));

    // The last member leaves: the lane stays open (never blocked), flagged, and the
    // creator is told; the summary lists it.
    s.leave("second@shop").unwrap();
    let lane = s.lane("api@shop", None).unwrap().lane;
    assert_eq!(lane.status, LaneStatus::Working, "never auto-blocked");
    assert!(lane.leaderless_since.is_some());
    assert!(s
        .events(500)
        .unwrap()
        .iter()
        .any(|e| e.kind == "lane.leaderless"));
    let creator = s.inbox("manager", InboxQuery::default()).unwrap();
    assert!(creator
        .messages
        .iter()
        .any(|m| m.message.subject == "[lane api] has no lead"));
    let sum = s.daily_summary(None, None).unwrap();
    assert!(
        sum.attention.iter().any(|a| a.contains("api (no lead")),
        "{:?}",
        sum.attention
    );
    // Naming a lead clears the flag.
    s.update_lane(
        LaneUpdate {
            agent: "manager".into(),
            lane: "api@shop".into(),
            lead: Some("claude@shop".into()),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    assert!(s
        .lane("api@shop", None)
        .unwrap()
        .lane
        .leaderless_since
        .is_none());
}

#[test]
fn idle_expiry_follows_the_node_setting() {
    let (s, clock) = hub();
    assert_eq!(s.setting("remote_idle_hours").unwrap().value, "4");
    assert_eq!(
        s.set_setting("remote_idle_hours", "0").unwrap_err().code(),
        "invalid"
    );
    let fixed = join(&s, &invite(&s, |i| i.agent = "fixed".into()).code).unwrap(); // 2 h
    let j = join(
        &s,
        &invite(&s, |i| {
            i.agent = "follows".into();
            i.idle_hours = None;
        })
        .code,
    )
    .unwrap();
    assert_eq!(
        s.remote_agents()
            .unwrap()
            .iter()
            .find(|r| r.agent == "follows@shop")
            .unwrap()
            .idle_hours,
        4
    );
    // Lowered to 1 h: existing agents without their own limit follow it.
    s.set_setting("remote_idle_hours", "1").unwrap();
    advance(&clock, 70 * 60_000);
    s.sweep().unwrap();
    assert!(s.verify_token(&j.token).is_err(), "gone after 1 h idle");
    assert!(s.verify_token(&fixed.token).is_ok(), "its invite said 2 h");
}

#[test]
fn managers_can_kick_over_mcp_within_their_projects() {
    let (s, _) = hub();
    for (name, program) in [("boss", Program::GrokBot), ("owner", Program::Human)] {
        s.register_agent_trusted(RegisterAgent {
            name: name.into(),
            program,
            project: Some("/work/shop".into()),
            model: None,
            description: None,
        })
        .unwrap();
    }
    let j = join(&s, &invite(&s, |_| {}).code).unwrap();
    // Not a manager: refused.
    let e = s
        .kick_agent_as("claude@shop", "cloud@shop", None, None)
        .unwrap_err();
    assert_eq!(e.code(), "forbidden");
    // A remote manager scoped to shop: vault agents don't exist for it, people can't go.
    let t = s
        .create_token("boss@shop", &["/work/shop".into()], None, None)
        .unwrap();
    let scope = s.verify_token(&t.secret).unwrap();
    let e = s
        .kick_agent_as("boss@shop", "vaulter@vault", None, Some(&scope))
        .unwrap_err();
    assert_eq!(e.code(), "not_found");
    let e = s
        .kick_agent_as("boss@shop", "owner@shop", None, Some(&scope))
        .unwrap_err();
    assert_eq!(e.code(), "forbidden");
    // ...and it can't act as another agent.
    let e = s
        .kick_agent_as("claude@shop", "cloud@shop", None, Some(&scope))
        .unwrap_err();
    assert_eq!(e.code(), "forbidden");
    let d = s
        .kick_agent_as(
            "boss@shop",
            "cloud@shop",
            Some("done for today"),
            Some(&scope),
        )
        .unwrap();
    assert_eq!(d.reason, "kicked: done for today");
    assert!(s.verify_token(&j.token).is_err());
    assert!(s
        .events(500)
        .unwrap()
        .iter()
        .all(|e| !e.payload.to_string().contains("done for today")));
}

#[test]
fn round3_scoped_kick_and_persistent_leave() {
    let (s, _) = hub();
    s.register_agent_trusted(RegisterAgent {
        name: "boss".into(),
        program: Program::GrokBot,
        project: Some("/work/shop".into()),
        model: None,
        description: None,
    })
    .unwrap();
    let hold_in = |agent: &str, project: &str, path: &str| {
        s.acquire_holds(AcquireHolds {
            agent: agent.into(),
            project: project.into(),
            paths: vec![path.into()],
            ttl_seconds: Some(3600),
            exclusive: true,
            reason: None,
            lane: None,
        })
        .unwrap();
    };
    // A shop-scoped manager kicks an agent that also holds a vault file: the vault hold stays.
    join(&s, &invite(&s, |i| i.agent = "wide".into()).code).unwrap();
    hold_in("wide@shop", "shop", "src/w.rs");
    hold_in("wide@shop", "vault", "keys/w.txt");
    let t = s
        .create_token("boss@shop", &["/work/shop".into()], None, None)
        .unwrap();
    let scope = s.verify_token(&t.secret).unwrap();
    let d = s
        .kick_agent_as("boss@shop", "wide@shop", None, Some(&scope))
        .unwrap();
    assert_eq!(d.holds_released, 1);
    let left: Vec<String> = s
        .holds(HoldFilter::default())
        .unwrap()
        .into_iter()
        .filter(|h| h.agent == "wide@shop")
        .map(|h| h.project)
        .collect();
    assert_eq!(left, vec!["vault"]);

    // A persistent agent's leave ends its resource and spend leases like a session end.
    let j = join(
        &s,
        &invite(&s, |i| {
            i.agent = "keeper".into();
            i.ephemeral = false;
        })
        .code,
    )
    .unwrap();
    let pscope = s.verify_token(&j.token).unwrap();
    s.register_resource(RegisterResource {
        agent: "keeper@shop".into(),
        kind: ResourceKind::DevServer,
        name: "vite@5173".into(),
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
    s.create_spend_account(
        NewSpendAccount {
            name: "cpu".into(),
            kind: SpendKind::Server,
            unit: "USD".into(),
            budget: None,
            period: SpendPeriod::Monthly,
            reset_day: 1,
            cost_per_hour: Some(1.0),
            requires_approval: false,
            approver: None,
            project: Some("shop".into()),
            description: None,
        },
        None,
    )
    .unwrap();
    s.borrow_spend(
        BorrowSpend {
            agent: "keeper@shop".into(),
            account: "cpu".into(),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    let d = s.leave_scoped("keeper@shop", Some(&pscope)).unwrap();
    assert_eq!(d.leases_closed, 2, "{d:?}");
    assert!(s.spend_leases(false, None).unwrap().is_empty());
    assert!(s.verify_token(&j.token).is_ok(), "persistent: token kept");
}
