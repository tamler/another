//! Lane-aware holds: lane assignment, same-lane overlaps as warnings, cross-lane conflicts
//! and events, the holds-by-lane view (scoped), expiry/renewal, and upgrade from v8.

use another_core::*;

type Handle = std::sync::Arc<std::sync::atomic::AtomicI64>;

fn store() -> (Store, Handle) {
    let (clock, h) = Clock::manual(1_700_000_000_000);
    let s = Store::open_in_memory(clock).unwrap();
    s.ensure_project("t", "/work/shop").unwrap();
    s.ensure_project("t", "/work/vault").unwrap();
    for (name, project) in [
        ("manager", None),
        ("claude", Some("/work/shop")),
        ("helper", Some("/work/shop")),
        ("codex", Some("/work/shop")),
        ("solo", Some("/work/shop")),
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
    for (lane, project, lead) in [
        ("api", "shop", "claude@shop"),
        ("billing", "shop", "codex@shop"),
        ("keys", "vault", "vaulter@vault"),
    ] {
        s.create_lane(
            NewLane {
                agent: "manager".into(),
                name: lane.into(),
                project: project.into(),
                lead: lead.into(),
                charter: String::new(),
                status: Some(LaneStatus::Working),
            },
            None,
        )
        .unwrap();
    }
    (s, h)
}

fn hold(s: &Store, agent: &str, project: &str, path: &str, lane: Option<&str>) -> HoldReport {
    s.acquire_holds(AcquireHolds {
        agent: agent.into(),
        project: project.into(),
        paths: vec![path.into()],
        ttl_seconds: Some(600),
        exclusive: true,
        reason: None,
        lane: lane.map(Into::into),
    })
    .unwrap()
}

fn shared(s: &Store, agent: &str, path: &str, lane: &str) -> HoldReport {
    s.acquire_holds(AcquireHolds {
        agent: agent.into(),
        project: "shop".into(),
        paths: vec![path.into()],
        ttl_seconds: Some(600),
        exclusive: false,
        reason: None,
        lane: Some(lane.into()),
    })
    .unwrap()
}

#[test]
fn lanes_decide_what_conflicts() {
    let (s, _) = store();
    // The lead's hold defaults to the lane it leads.
    let r = hold(&s, "claude@shop", "shop", "src/api/routes.rs", None);
    assert_eq!(r.granted[0].lane.as_deref(), Some("api"));

    // A lane-mate's exclusive hold on the same path is blocked: two exclusive holds block
    // each other even inside a lane (Jacob's decision).
    let clash = hold(&s, "helper@shop", "shop", "src/api/**", Some("api"));
    assert!(clash.granted.is_empty());
    assert_eq!(clash.conflicts.len(), 1);
    assert_eq!(clash.conflicts[0].held.agent, "claude@shop");
    assert!(!clash.conflicts[0].same_lane);
    // A lane-mate's *shared* hold overlapping: not blocked, a same-lane warning.
    let mate = shared(&s, "helper@shop", "src/api/**", "api");
    assert_eq!(mate.granted.len(), 1);
    assert!(mate.conflicts.is_empty());
    assert_eq!(mate.warnings.len(), 1);
    assert!(mate.warnings[0].same_lane);
    assert_eq!(mate.warnings[0].held.agent, "claude@shop");

    // Another lane: a cross-lane conflict, with both lanes in the events.
    let other = hold(&s, "codex@shop", "shop", "src/api/routes.rs", None);
    assert!(other.granted.is_empty());
    assert_eq!(other.conflicts.len(), 2, "both api-lane holds overlap");
    assert!(other
        .conflicts
        .iter()
        .all(|c| c.held.lane.as_deref() == Some("api") && !c.same_lane));
    let events = s.events(500).unwrap();
    let conflict = events.iter().rfind(|e| e.kind == "hold.conflict").unwrap();
    let billing = s.lane("billing", None).unwrap().lane.lane_id;
    let api = s.lane("api", None).unwrap().lane.lane_id;
    assert_eq!(conflict.payload["lane_id"], billing.as_str());
    assert_eq!(conflict.payload["held_lane_ids"][0], api.as_str());
    let lane_conflict = events
        .iter()
        .rfind(|e| e.kind == "lane.hold_conflict")
        .expect("lane.hold_conflict");
    assert_eq!(lane_conflict.payload["other_lane_id"], api.as_str());
    let feed = s
        .activity(ActivityQuery {
            kinds: vec![ActivityKind::Lane],
            ..Default::default()
        })
        .unwrap();
    assert!(feed.items.iter().any(|i| i
        .summary
        .starts_with("lane billing ran into lane api on src/api/routes.rs")));

    // No lane on the requester: blocked as before.
    let solo = hold(&s, "solo@shop", "shop", "src/api/routes.rs", None);
    assert_eq!(solo.conflicts.len(), 2);
    assert_eq!(
        s.events(500)
            .unwrap()
            .iter()
            .filter(|e| e.kind == "lane.hold_conflict")
            .count(),
        1,
        "lane.hold_conflict only when both sides have a lane"
    );

    // Lane checks: another project's lane, or a closed one.
    let wrong = s.acquire_holds(AcquireHolds {
        agent: "claude@shop".into(),
        project: "shop".into(),
        paths: vec!["x".into()],
        ttl_seconds: None,
        exclusive: true,
        reason: None,
        lane: Some("keys".into()),
    });
    assert_eq!(
        wrong.unwrap_err().code(),
        "not_found",
        "another project's lane is looked up in this project only"
    );
    s.close_lane("manager", "billing", true, None, None)
        .unwrap();
    let closed = s.acquire_holds(AcquireHolds {
        agent: "codex@shop".into(),
        project: "shop".into(),
        paths: vec!["y".into()],
        ttl_seconds: None,
        exclusive: true,
        reason: None,
        lane: Some("billing".into()),
    });
    assert_eq!(closed.unwrap_err().code(), "conflict");
}

#[test]
fn without_lanes_nothing_changes() {
    let (s, _) = store();
    let a = hold(&s, "solo@shop", "shop", "README.md", None);
    assert_eq!(a.granted[0].lane, None);
    let b = hold(&s, "helper@shop", "shop", "README.md", None);
    assert_eq!(b.conflicts.len(), 1);
    assert!(!b.conflicts[0].same_lane);
    assert!(s
        .events(500)
        .unwrap()
        .iter()
        .all(|e| e.kind != "lane.hold_conflict"));
}

#[test]
fn holds_by_lane_view_scope_and_lifetime() {
    let (s, clock) = store();
    hold(&s, "claude@shop", "shop", "src/api/a.rs", None);
    hold(&s, "helper@shop", "shop", "src/api/b.rs", Some("api"));
    hold(&s, "codex@shop", "shop", "src/billing.rs", None);
    hold(&s, "solo@shop", "shop", "docs/x.md", None);
    hold(&s, "vaulter@vault", "vault", "keys.toml", None);

    let groups = s.holds_by_lane(HoldFilter::default(), None).unwrap();
    let names: Vec<Option<&str>> = groups.iter().map(|g| g.lane.as_deref()).collect();
    assert_eq!(
        names,
        vec![Some("api"), Some("billing"), Some("keys"), None]
    );
    assert_eq!(groups[0].holds.len(), 2);
    assert_eq!(groups[0].lead.as_deref(), Some("claude@shop"));
    assert_eq!(groups[0].status, Some(LaneStatus::Working));
    assert_eq!(groups[3].project, "shop");

    // A token scoped to shop never sees the vault lane's holds.
    let t = s
        .create_token("manager", &["/work/shop".into()], None, None)
        .unwrap();
    let scope = s.verify_token(&t.secret).unwrap();
    let scoped = s
        .holds_by_lane(HoldFilter::default(), Some(&scope))
        .unwrap();
    assert!(scoped.iter().all(|g| g.project == "shop"));
    assert_eq!(scoped.len(), 3);

    // lane_show lists the lane's live holds.
    assert_eq!(s.lane("api", None).unwrap().holds.len(), 2);

    // Holds still expire, and heartbeats renew them (lane kept).
    advance(&clock, 300_000);
    s.heartbeat("claude@shop", false).unwrap();
    advance(&clock, 400_000);
    s.sweep().unwrap();
    let api = s.lane("api", None).unwrap();
    assert_eq!(api.holds.len(), 1, "helper's expired, claude's was renewed");
    assert_eq!(api.holds[0].agent, "claude@shop");
    assert_eq!(api.holds[0].lane.as_deref(), Some("api"));
}

fn advance(h: &Handle, ms: i64) {
    h.fetch_add(ms, std::sync::atomic::Ordering::SeqCst);
}

#[test]
fn upgrades_a_database_created_before_lane_holds() {
    let dir = tempfile::tempdir().unwrap();
    {
        let old = Store::open_at_schema(dir.path(), 8).unwrap();
        old.ensure_project("t", "/work/shop").unwrap();
        old.register_agent(RegisterAgent {
            name: "a".into(),
            program: Program::Other,
            project: Some("/work/shop".into()),
            model: None,
            description: None,
        })
        .unwrap();
    }
    let s = Store::open(dir.path()).unwrap();
    let r = hold(&s, "a@shop", "shop", "x.rs", None);
    assert_eq!(r.granted[0].lane, None);
    assert!(Store::schema_version() >= 9);
}

#[test]
fn hold_lanes_resolve_within_the_holds_project() {
    let (s, _) = store();
    let take = |lane: &str| {
        s.acquire_holds(AcquireHolds {
            agent: "claude@shop".into(),
            project: "shop".into(),
            paths: vec!["x.rs".into()],
            ttl_seconds: None,
            exclusive: true,
            reason: None,
            lane: Some(lane.into()),
        })
        .unwrap_err()
    };
    // The vault project's lane, by name, by name@project and by id: all "unknown lane",
    // exactly like a lane that doesn't exist.
    let keys_id = s.lane("keys", None).unwrap().lane.lane_id;
    let missing = take("nope");
    for (key, err) in [
        ("keys", take("keys")),
        ("keys@vault", take("keys@vault")),
        (keys_id.as_str(), take(&keys_id)),
    ] {
        assert_eq!(err.code(), "not_found", "{key}");
        assert_eq!(
            err.to_string().replace(key, "X"),
            missing.to_string().replace("nope", "X"),
            "{key}"
        );
    }
}

#[test]
fn cross_lane_conflicts_mail_the_holding_lead() {
    let (s, _) = store();
    hold(&s, "helper@shop", "shop", "src/api/x.rs", Some("api"));
    for _ in 0..3 {
        let r = hold(&s, "codex@shop", "shop", "src/api/x.rs", None);
        assert!(r.granted.is_empty());
    }
    // The api lane's lead (claude) hears about it once (debounced), from the system agent.
    let inbox = s.inbox("claude@shop", InboxQuery::default()).unwrap();
    let mails: Vec<_> = inbox
        .messages
        .iter()
        .filter(|m| m.message.subject == "[lane api] hold conflict with lane billing")
        .collect();
    assert_eq!(
        mails.len(),
        1,
        "{:?}",
        inbox
            .messages
            .iter()
            .map(|m| &m.message.subject)
            .collect::<Vec<_>>()
    );
    assert_eq!(mails[0].message.from, SYSTEM_AGENT);
    let body = &s.message(&mails[0].message.message_id).unwrap().body;
    assert!(
        body.contains("codex@shop (lane billing)") && body.contains("src/api/x.rs"),
        "{body}"
    );
    // Events and the webhook filter keep working alongside.
    let events = s.events(500).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == "lane.hold_conflict")
            .count(),
        3
    );
}

#[test]
fn scoped_holds_by_lane_filters_never_resolve_node_wide() {
    let (s, clock) = store();
    // "twin" exists in both projects: a node-wide lookup would be ambiguous and name both.
    for p in ["/work/shop", "/work/vault"] {
        s.register_agent(RegisterAgent {
            name: "twin".into(),
            program: Program::Claude,
            project: Some(p.into()),
            model: None,
            description: None,
        })
        .unwrap();
    }
    hold(&s, "twin@shop", "shop", "src/t.rs", None);
    hold(&s, "twin@vault", "vault", "keys/t.rs", None);
    let t = s
        .create_token("manager", &["/work/shop".into()], None, None)
        .unwrap();
    let scope = s.verify_token(&t.secret).unwrap();
    let by = |agent: Option<&str>, project: Option<&str>| {
        s.holds_by_lane(
            HoldFilter {
                agent: agent.map(Into::into),
                project: project.map(Into::into),
            },
            Some(&scope),
        )
    };
    let g = by(Some("twin"), None).unwrap();
    let all: Vec<String> = g
        .iter()
        .flat_map(|g| g.holds.iter().map(|h| h.agent.clone()))
        .collect();
    assert_eq!(all, vec!["twin@shop"]);
    // A hidden project and a missing one look the same: an empty list.
    assert!(by(None, Some("vault")).unwrap().is_empty());
    assert!(by(None, Some("nowhere")).unwrap().is_empty());
    assert!(by(Some("vaulter@vault"), None).unwrap().is_empty());
    assert_eq!(by(None, Some("/work/shop")).unwrap().len(), 1);

    // NO_LANE: an explicit "no lane" never falls back to the lane the agent leads.
    let r = hold(&s, "claude@shop", "shop", "src/plain.rs", Some(NO_LANE));
    assert_eq!(r.granted[0].lane, None);
    let r = hold(&s, "claude@shop", "shop", "src/inferred.rs", None);
    assert_eq!(r.granted[0].lane.as_deref(), Some("api"));

    // lane show never lists an expired hold, even before anything else sweeps.
    advance(&clock, 3_600_000);
    assert!(s.lane("api", None).unwrap().holds.is_empty());
}
