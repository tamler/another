//! Federation tie-in for lanes, decisions and lane-aware holds (07-remote-agents.md §5): a hub
//! and a spoke, each with its own ANOTHER_HOME and its own HTTP server on a different port,
//! plus a third node to prove relayed answers reach only the decision's node.

use std::net::SocketAddr;

use another::fed;
use another::serve::{http_router, HttpOptions};
use another_core::*;
use tokio_util::sync::CancellationToken;

struct Node {
    _dir: tempfile::TempDir,
    store: Store,
    url: String,
    ct: CancellationToken,
    _rt: tokio::runtime::Runtime,
}

impl Drop for Node {
    fn drop(&mut self) {
        self.ct.cancel();
    }
}

fn node(name: &str, federation: bool) -> Node {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.set_node_name(name).unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let ct = CancellationToken::new();
    let app = http_router(
        store.clone(),
        ct.clone(),
        &HttpOptions {
            auth: true,
            federation,
            ..Default::default()
        },
    );
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let addr = listener.local_addr().unwrap();
    assert_ne!(addr.port(), 7466);
    let stop = ct.clone();
    rt.spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { stop.cancelled_owned().await })
        .await
        .unwrap();
    });
    Node {
        _dir: dir,
        store,
        url: format!("http://{addr}"),
        ct,
        _rt: rt,
    }
}

fn agent(s: &Store, name: &str, program: Program, project: Option<&str>) {
    s.register_agent(RegisterAgent {
        name: name.into(),
        program,
        project: project.map(Into::into),
        model: None,
        description: None,
    })
    .unwrap();
}

fn pair(hub: &Node, spoke: &Node) {
    let code = hub
        .store
        .create_invite(&hub.url, &[Perm::ReadFeed, Perm::SendMail], 3600)
        .unwrap();
    fed::join(&spoke.store, &code).unwrap();
}

fn sync(n: &Node) {
    let r = fed::sync(&n.store).unwrap();
    assert!(r.push_rejected.is_empty(), "{:?}", r.push_rejected);
    assert!(r.pull_rejected.is_empty(), "{:?}", r.pull_rejected);
}

fn lane(s: &Store, agent: &str, name: &str, project: &str, lead: &str) -> LaneDetail {
    s.create_lane(
        NewLane {
            agent: agent.into(),
            name: name.into(),
            project: project.into(),
            lead: lead.into(),
            charter: "hub-only charter text".into(),
            status: Some(LaneStatus::Working),
        },
        None,
    )
    .unwrap()
}

fn hold(s: &Store, agent: &str, lane: &str) -> HoldReport {
    s.acquire_holds(AcquireHolds {
        agent: agent.into(),
        project: "shop".into(),
        paths: vec!["src/api.rs".into()],
        ttl_seconds: None,
        exclusive: true,
        reason: None,
        lane: Some(lane.into()),
    })
    .unwrap()
}

#[test]
fn lanes_decisions_and_holds_across_nodes() {
    let hub = node("hub", true);
    let spoke = node("laptop", false);
    let third = node("desk", false);
    assert_ne!(hub.url, spoke.url, "separate servers on different ports");
    hub.store.ensure_project("t", "/work/shop").unwrap();
    agent(&hub.store, "manager", Program::GrokBot, None);
    agent(&hub.store, "claude", Program::Claude, Some("/work/shop"));
    spoke.store.ensure_project("t", "/src/shop").unwrap();
    agent(&spoke.store, "codex", Program::Codex, Some("/src/shop"));
    third.store.ensure_project("t", "/x/shop").unwrap();
    let hub_lane = lane(&hub.store, "manager", "api", "shop", "claude@shop");
    pair(&hub, &spoke);
    pair(&hub, &third);
    sync(&spoke);
    sync(&third);

    // 1. The spoke mirrors the hub's lane: name, project, lead, status; no charter.
    let mirror = spoke.store.lane("api", None).unwrap().lane;
    assert_eq!(mirror.lane_id, hub_lane.lane.lane_id);
    assert_eq!(mirror.node.as_deref(), Some("hub"));
    assert_eq!(mirror.status, LaneStatus::Working);
    assert_eq!(mirror.charter, "", "events never carry the charter");

    // 2. Only the owner writes. A report into the hub's lane from the spoke is a proposal,
    //    applied by the hub when it arrives; closing it from the spoke is refused.
    let proposed = spoke
        .store
        .report_lane(
            LaneReportInput {
                agent: "codex@shop".into(),
                lane: mirror.lane_id.clone(),
                kind: ReportKind::Status,
                summary: "from the laptop".into(),
                progress: Some(30),
                status: None,
                links: vec![],
                artifacts: vec![],
                outcome: None,
                pr_url: None,
            },
            None,
        )
        .unwrap();
    assert_eq!(proposed.proposed_to.as_deref(), Some("hub"));
    let e = spoke
        .store
        .close_lane("codex@shop", &mirror.lane_id, false, None, None)
        .unwrap_err();
    assert_eq!(e.code(), "conflict");
    assert!(e.to_string().contains("owned by node hub"), "{e}");
    sync(&spoke);
    let applied = hub.store.lane("api", None).unwrap();
    let r = applied
        .reports
        .iter()
        .find(|r| r.summary == "from the laptop")
        .expect("the hub applied the proposal");
    assert!(r.agent.starts_with("codex@shop"), "{}", r.agent);
    assert_eq!(r.progress, Some(30));
    // The proposal event carries ids and status only, and never reaches the third node.
    let ev = spoke
        .store
        .events(10_000)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "lane.report_proposed")
        .unwrap();
    assert!(!ev.payload.to_string().contains("from the laptop"));
    // Decisions can't flip a mirror to needs_decision either: ask on the owner node.
    let asked = spoke.store.ask_decision(
        NewDecision {
            agent: "codex@shop".into(),
            question: "Ship it?".into(),
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
            lane: Some(mirror.lane_id.clone()),
            ..Default::default()
        },
        None,
    );
    let e = asked.unwrap_err();
    assert_eq!(e.code(), "conflict");
    assert!(e.to_string().contains("owned by node hub"), "{e}");
    assert_eq!(
        spoke.store.lane(&mirror.lane_id, None).unwrap().lane.status,
        LaneStatus::Working
    );
    // The owner's changes replay onto the mirror in order.
    for to in [
        LaneStatus::Blocked,
        LaneStatus::NeedsDecision,
        LaneStatus::Working,
    ] {
        hub.store
            .update_lane(
                LaneUpdate {
                    agent: "manager".into(),
                    lane: "api".into(),
                    status: Some(to),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
    }
    sync(&spoke);
    assert_eq!(
        spoke.store.lane(&mirror.lane_id, None).unwrap().lane.status,
        LaneStatus::Working
    );

    // 3. Same name on two nodes: each node's name means its own lane.
    let local = lane(&spoke.store, "codex@shop", "api", "/src/shop", "codex@shop");
    assert_eq!(
        spoke.store.lane("api", None).unwrap().lane.lane_id,
        local.lane.lane_id
    );
    assert_eq!(spoke.store.lane("api@shop", None).unwrap().lane.node, None);
    sync(&spoke);
    assert_eq!(
        hub.store.lane("api", None).unwrap().lane.lane_id,
        hub_lane.lane.lane_id
    );

    // 4. Lane-aware holds across nodes: warnings (never blocking), same_lane when both
    //    holds are for the same (mirrored) lane.
    assert!(hold(&hub.store, "claude@shop", "api").granted.len() == 1);
    sync(&spoke);
    let same = hold(&spoke.store, "codex@shop", &hub_lane.lane.lane_id);
    assert_eq!(same.granted.len(), 1, "cross-node clashes never block");
    assert!(
        same.warnings
            .iter()
            .any(|w| w.same_lane && w.held.lane.as_deref() == Some("api")),
        "{:?}",
        same.warnings
    );
    spoke.store.release_holds("codex@shop", &[]).unwrap();
    let other = hold(&spoke.store, "codex@shop", "api");
    assert!(
        other.warnings.iter().all(|w| !w.same_lane),
        "different lanes"
    );

    // 5. A decision asked on the spoke is answered on the hub and flows back.
    let d = spoke
        .store
        .ask_decision(
            NewDecision {
                agent: "codex@shop".into(),
                question: "Use the hub's schema?".into(),
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
                allow_free_text: true,
                ..Default::default()
            },
            None,
        )
        .unwrap();
    sync(&spoke);
    let seen = hub
        .store
        .decisions(DecisionFilter::default(), None)
        .unwrap();
    let m = seen
        .iter()
        .find(|x| x.decision_id == d.decision_id)
        .expect("mirrored on the hub");
    assert_eq!(m.node.as_deref(), Some("laptop"));
    assert_eq!(m.asker, "codex@shop@laptop");
    let relayed = hub
        .store
        .answer_decision(
            AnswerDecision {
                decision: d.decision_id.clone(),
                value: Some("yes".into()),
                text: Some("private note for the laptop".into()),
                answered_by: Some("owner".into()),
                relayed_by: Some("manager".into()),
            },
            None,
        )
        .unwrap();
    assert_eq!(relayed.status, DecisionStatus::AnswerSent);
    let twice = hub.store.answer_decision(
        AnswerDecision {
            decision: d.decision_id.clone(),
            value: Some("no".into()),
            ..Default::default()
        },
        None,
    );
    assert_eq!(twice.unwrap_err().code(), "conflict", "first answer wins");
    sync(&spoke); // pull: the spoke applies the relayed answer
    let done = spoke.store.decision(&d.decision_id, None).unwrap();
    assert_eq!(done.status, DecisionStatus::Answered);
    let a = done.answer.unwrap();
    assert_eq!(a.value.as_deref(), Some("yes"));
    assert_eq!(a.answered_by, "owner");
    assert!(a.relayed_by.unwrap().starts_with("manager"));
    let inbox = spoke
        .store
        .inbox("codex@shop", InboxQuery::default())
        .unwrap();
    let mail = &inbox.messages[0].message;
    assert!(mail.subject.starts_with("[decision] Yes"));
    assert!(mail.body.contains("private note for the laptop"));
    // The text crossed as mail, already marked read: the answer mail is what the agent reads.
    let carrier = inbox
        .messages
        .iter()
        .find(|m| m.message.subject.starts_with("[decision] answer note"))
        .expect("carrier mail");
    assert!(carrier.read_at.is_some());
    // The relay event itself carries ids and status only.
    let relay = hub
        .store
        .events(10_000)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "decision.answer_relayed")
        .unwrap();
    assert_eq!(relay.payload["has_text"], true);
    assert!(
        !relay.payload.to_string().contains("private note"),
        "{}",
        relay.payload
    );
    sync(&spoke); // push: the hub's mirror settles
    let settled = hub.store.decision(&d.decision_id, None).unwrap();
    assert_eq!(settled.status, DecisionStatus::Answered);

    // The relayed answer (with its free text) never reached the third node.
    sync(&third);
    let third_events = third.store.events(10_000).unwrap();
    assert!(third_events
        .iter()
        .all(|e| e.kind != "decision.answer_relayed" && e.kind != "lane.report_proposed"));
    assert!(!serde_json::to_string(&third_events)
        .unwrap()
        .contains("private note"));
    // ...and the hub's own feed shows no text either.
    let feed = hub
        .store
        .activity(ActivityQuery {
            limit: 500,
            ..Default::default()
        })
        .unwrap();
    assert!(!serde_json::to_string(&feed)
        .unwrap()
        .contains("private note"));
}
