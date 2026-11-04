//! Paths the r4 coverage pass found with no test: upgrading a database from before node keys,
//! renewing holds, re-claiming a request, releasing a lease you don't hold, and which tokens can
//! see resource events. Tests only; anything surprising is recorded in the lane's BUGS-FOUND
//! list.

use std::sync::atomic::Ordering;

use another_core::*;
use rusqlite::{params, Connection};

const T0: i64 = 1_800_000_000_000;

type Handle = std::sync::Arc<std::sync::atomic::AtomicI64>;

fn register(s: &Store, name: &str, project: Option<&str>) {
    s.register_agent_trusted(RegisterAgent {
        name: name.into(),
        program: Program::Claude,
        project: project.map(Into::into),
        model: None,
        description: None,
    })
    .unwrap();
}

fn store() -> (Store, Handle) {
    let (clock, h) = Clock::manual(T0);
    let s = Store::open_in_memory(clock).unwrap();
    s.ensure_project("t", "/work/shop").unwrap();
    s.ensure_project("t", "/work/vault").unwrap();
    register(&s, "manager", None);
    register(&s, "shopper", Some("/work/shop"));
    register(&s, "vaulter", Some("/work/vault"));
    (s, h)
}

fn scope(s: &Store, glob: &str) -> TokenScope {
    let t = s
        .create_token("manager", &[glob.to_string()], None, None)
        .unwrap();
    s.verify_token(&t.secret).unwrap()
}

// ---------------------------------------------------------------------------------------
// Opening a database from before node keys existed

#[test]
fn opening_a_database_without_a_node_key_creates_one_and_signs_old_events() {
    let dir = tempfile::tempdir().unwrap();
    let node_id = {
        let s = Store::open(dir.path()).unwrap();
        s.ensure_project("t", "/work/shop").unwrap();
        register(&s, "shopper", Some("/work/shop"));
        s.node_id().to_string()
    };
    let db = dir.path().join("another.db");
    {
        let c = Connection::open(&db).unwrap();
        c.execute(
            "UPDATE nodes SET secret_key = NULL, public_key = NULL WHERE node_id = ?1",
            params![node_id],
        )
        .unwrap();
        let unsigned = c
            .execute(
                "UPDATE events SET sig = NULL WHERE node_id = ?1",
                params![node_id],
            )
            .unwrap();
        assert!(unsigned > 0);
    }

    let reopened = Store::open(dir.path()).unwrap();
    assert_eq!(reopened.node_id(), node_id, "the node keeps its identity");

    let c = Connection::open(&db).unwrap();
    let public: String = c
        .query_row(
            "SELECT public_key FROM nodes WHERE node_id = ?1",
            params![node_id],
            |r| r.get(0),
        )
        .unwrap();
    let mut stmt = c
        .prepare("SELECT event_id, ts, actor, kind, payload, sig FROM events WHERE node_id = ?1")
        .unwrap();
    let rows: Vec<(String, i64, String, String, String, Option<String>)> = stmt
        .query_map(params![node_id], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(!rows.is_empty());
    for (id, ts, actor, kind, payload, sig) in rows {
        let sig = sig.unwrap_or_else(|| panic!("{kind} is still unsigned"));
        let message = crypto::event_message(&id, &node_id, ts, &actor, &kind, &payload);
        assert!(crypto::verify(&public, &message, &sig), "{kind}");
    }

    // A third open changes nothing: the key is stable from here on.
    drop(reopened);
    Store::open(dir.path()).unwrap();
    let again: String = c
        .query_row(
            "SELECT public_key FROM nodes WHERE node_id = ?1",
            params![node_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(again, public);
}

// ---------------------------------------------------------------------------------------
// Holds

fn hold(s: &Store, agent: &str, path: &str, ttl: i64) -> Hold {
    s.acquire_holds(AcquireHolds {
        agent: agent.into(),
        project: "shop".into(),
        paths: vec![path.into()],
        ttl_seconds: Some(ttl),
        exclusive: true,
        reason: None,
        lane: None,
    })
    .unwrap()
    .granted
    .remove(0)
}

#[test]
fn renewing_holds_extends_only_the_agents_own() {
    let (s, h) = store();
    register(&s, "other", Some("/work/shop"));
    let a = hold(&s, "shopper@shop", "src/a.rs", 60);
    let b = hold(&s, "shopper@shop", "src/b.rs", 60);
    let theirs = hold(&s, "other@shop", "src/c.rs", 60);

    h.fetch_add(30_000, Ordering::SeqCst);
    let renewed = s
        .renew_holds("shopper@shop", std::slice::from_ref(&a.hold_id))
        .unwrap();
    assert_eq!(renewed.len(), 1);
    assert_eq!(renewed[0].hold_id, a.hold_id);
    assert!(renewed[0].expires_at > a.expires_at, "{renewed:?}");

    // Someone else's hold can't be renewed by id.
    assert!(s
        .renew_holds("shopper@shop", std::slice::from_ref(&theirs.hold_id))
        .is_err());

    // With no ids, all of the agent's live holds; the renewed one outlasts the deadline
    // the first two started with.
    let all = s.renew_holds("shopper@shop", &[]).unwrap();
    let mut ids: Vec<_> = all.iter().map(|x| x.hold_id.clone()).collect();
    ids.sort();
    let mut want = vec![a.hold_id.clone(), b.hold_id.clone()];
    want.sort();
    assert_eq!(ids, want);

    h.fetch_add(45_000, Ordering::SeqCst);
    s.sweep().unwrap();
    assert!(
        s.acquire_holds(AcquireHolds {
            agent: "other@shop".into(),
            project: "shop".into(),
            paths: vec!["src/a.rs".into()],
            ttl_seconds: Some(60),
            exclusive: true,
            reason: None,
            lane: None,
        })
        .unwrap()
        .granted
        .is_empty(),
        "the renewal kept the hold alive past its first deadline"
    );
}

// ---------------------------------------------------------------------------------------
// Requests

#[test]
fn claiming_your_own_request_again_only_moves_its_due_time() {
    let (s, _) = store();
    let sent = s
        .send_message(SendMessage {
            from: "manager".into(),
            to: vec!["shopper@shop".into()],
            subject: "please look".into(),
            body: "at this".into(),
            request: Some(NewRequest::default()),
            ..Default::default()
        })
        .unwrap();
    let rid = sent.request_id.clone().expect("a request was opened");
    let update = |action, due| {
        s.update_request(RequestUpdate {
            agent: "shopper@shop".into(),
            request_id: rid.clone(),
            action,
            note: None,
            due_in_seconds: due,
        })
    };
    let first = update(RequestAction::Claim, Some(3600)).unwrap();
    let due = first.due_at.clone().expect("a due time");

    let later = update(RequestAction::Claim, Some(7200)).unwrap();
    assert_eq!(later.status, RequestStatus::Claimed);
    assert_eq!(later.owner, first.owner);
    assert!(later.due_at.clone().unwrap() > due, "{later:?}");

    // Without a new time, nothing moves.
    let same = update(RequestAction::Claim, None).unwrap();
    assert_eq!(same.due_at, later.due_at);
}

// ---------------------------------------------------------------------------------------
// Resources

fn gpu(s: &Store, agent: &str, project: &str, name: &str) -> LeaseOutcome {
    s.register_resource(RegisterResource {
        agent: agent.into(),
        kind: ResourceKind::ModelServer,
        name: name.into(),
        endpoint: None,
        port: None,
        pid: None,
        project: Some(project.into()),
        purpose: None,
        mode: LeaseMode::Shared,
        ttl_seconds: Some(600),
        metadata: None,
    })
    .unwrap()
}

/// Everything a token scoped to `glob` sees in the feed.
fn seen(s: &Store, scope: &TokenScope) -> String {
    let feed = s
        .activity_scoped(
            ActivityQuery {
                since: Some(Since::Time(0)),
                limit: 500,
                kinds: vec![],
            },
            Some(scope),
        )
        .unwrap();
    serde_json::to_string(&feed.items).unwrap()
}

#[test]
fn resource_events_reach_only_tokens_for_the_resources_project() {
    let (s, _) = store();
    let shop = scope(&s, "/work/shop");
    let vault = scope(&s, "/work/vault");
    let in_shop = gpu(&s, "shopper@shop", "shop", "shop-gpu");
    let in_vault = gpu(&s, "vaulter@vault", "vault", "vault-gpu");
    s.release_resource(
        "shopper@shop",
        &ResourceRef::Id(in_shop.resource.resource_id.clone()),
        false,
    )
    .unwrap();
    s.release_resource(
        "vaulter@vault",
        &ResourceRef::Id(in_vault.resource.resource_id.clone()),
        false,
    )
    .unwrap();

    let for_shop = seen(&s, &shop);
    assert!(for_shop.contains("shop-gpu"), "{for_shop}");
    assert!(!for_shop.contains("vault-gpu"), "{for_shop}");
    assert!(!for_shop.contains(&in_vault.resource.resource_id));

    let for_vault = seen(&s, &vault);
    assert!(for_vault.contains("vault-gpu"), "{for_vault}");
    assert!(!for_vault.contains("shop-gpu"), "{for_vault}");
    assert!(!for_vault.contains(&in_shop.resource.resource_id));
}

#[test]
fn heartbeating_a_resource_you_do_not_hold_names_it() {
    let (s, _) = store();
    gpu(&s, "shopper@shop", "shop", "shop-gpu");
    let other = gpu(&s, "manager", "shop", "other-gpu");
    let e = s
        .heartbeat_resources(
            "shopper@shop",
            &[ResourceRef::Id(other.resource.resource_id.clone())],
        )
        .unwrap_err();
    assert_eq!(e.code(), "not_found", "{e}");
    assert!(e.to_string().contains(&other.resource.resource_id), "{e}");
}

// ---------------------------------------------------------------------------------------
// Lanes

fn lane(s: &Store) -> LaneDetail {
    s.create_lane(
        NewLane {
            agent: "manager".into(),
            name: "api".into(),
            project: "shop".into(),
            lead: "shopper@shop".into(),
            charter: "charter".into(),
            status: Some(LaneStatus::Working),
        },
        None,
    )
    .unwrap()
}

fn update(status: Option<LaneStatus>, links: Vec<(LaneLinkKind, String)>) -> LaneUpdate {
    LaneUpdate {
        agent: "manager".into(),
        lane: "api".into(),
        status,
        links,
        ..Default::default()
    }
}

#[test]
fn linking_to_a_lane_shows_in_the_feed_and_a_closed_lane_takes_no_changes() {
    let (s, _) = store();
    lane(&s);
    let m = s
        .send_message(SendMessage {
            from: "manager".into(),
            to: vec!["shopper@shop".into()],
            subject: "about the api".into(),
            body: "body".into(),
            ..Default::default()
        })
        .unwrap();
    s.update_lane(
        update(None, vec![(LaneLinkKind::Message, m.message_id.clone())]),
        None,
    )
    .unwrap();
    let feed = s
        .activity(ActivityQuery {
            since: Some(Since::Time(0)),
            limit: 500,
            kinds: vec![],
        })
        .unwrap();
    assert!(
        feed.items
            .iter()
            .any(|i| i.summary.contains("linked message")
                && i.summary.contains(&m.message_id)
                && i.summary.contains("api")),
        "{:?}",
        feed.items.iter().map(|i| &i.summary).collect::<Vec<_>>()
    );

    // Only `lane close` or a done report finishes a lane.
    let e = s
        .update_lane(update(Some(LaneStatus::Done), vec![]), None)
        .unwrap_err();
    assert_eq!(e.code(), "invalid", "{e}");
    s.close_lane("manager", "api", false, Some("shipped".into()), None)
        .unwrap();

    let e = s
        .update_lane(update(Some(LaneStatus::Working), vec![]), None)
        .unwrap_err();
    assert_eq!(e.code(), "conflict", "{e}");
    assert!(e.to_string().contains("lane api is done"), "{e}");
    // Links are still allowed: they add context to a closed lane.
    assert!(s
        .update_lane(
            update(None, vec![(LaneLinkKind::Message, m.message_id)]),
            None
        )
        .is_ok());
}

#[test]
fn lane_reports_keep_done_only_fields_on_done_reports() {
    let (s, _) = store();
    lane(&s);
    let input = || LaneReportInput {
        agent: "shopper@shop".into(),
        lane: "api".into(),
        kind: ReportKind::Status,
        summary: "going fine".into(),
        progress: None,
        status: None,
        links: vec![],
        artifacts: vec![],
        outcome: None,
        pr_url: None,
    };
    for (bad, wanted) in [
        (
            LaneReportInput {
                progress: Some(101),
                ..input()
            },
            "percentage",
        ),
        (
            LaneReportInput {
                outcome: Some("shipped".into()),
                ..input()
            },
            "belong on a done report",
        ),
        (
            LaneReportInput {
                status: Some(LaneStatus::Done),
                ..input()
            },
            "to finish a lane",
        ),
        (
            LaneReportInput {
                kind: ReportKind::Done,
                outcome: Some("   ".into()),
                ..input()
            },
            "needs an outcome",
        ),
    ] {
        let e = s.report_lane(bad, None).unwrap_err();
        assert_eq!(e.code(), "invalid", "{e}");
        assert!(e.to_string().contains(wanted), "{e}");
    }
    assert!(s.lane("api", None).unwrap().reports.is_empty());
    assert_eq!(
        s.lane("api", None).unwrap().lane.status,
        LaneStatus::Working
    );
}

#[test]
fn unknown_request_actions_and_link_kinds_list_the_valid_ones() {
    let e = "nonsense".parse::<RequestAction>().unwrap_err();
    assert!(e.to_string().contains("claim, release"), "{e}");
    let e = "nonsense".parse::<LaneLinkKind>().unwrap_err();
    assert!(e.to_string().contains("thread, request"), "{e}");
}
