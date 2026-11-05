//! What a node does with lane and decision changes it can't apply: refusals on a mirrored lane,
//! and relayed answers that arrive too late or break the asking node's rules. A hub and a spoke,
//! each with its own ANOTHER_HOME and HTTP server on an ephemeral loopback port.

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
    s.register_agent_trusted(RegisterAgent {
        name: name.into(),
        program,
        project: project.map(Into::into),
        model: None,
        description: None,
    })
    .unwrap();
}

/// A hub with a manager and a lead, a spoke with a worker and a person, paired and synced.
fn pair() -> (Node, Node) {
    let hub = node("hub", true);
    let spoke = node("laptop", false);
    hub.store.ensure_project("t", "/work/shop").unwrap();
    agent(&hub.store, "manager", Program::GrokBot, None);
    agent(&hub.store, "claude", Program::Claude, Some("/work/shop"));
    spoke.store.ensure_project("t", "/src/shop").unwrap();
    agent(&spoke.store, "codex", Program::Codex, Some("/src/shop"));
    agent(&spoke.store, "pat", Program::Human, None);
    let code = hub
        .store
        .create_invite(&hub.url, &[Perm::ReadFeed, Perm::SendMail], 3600)
        .unwrap();
    fed::join(&spoke.store, &code).unwrap();
    sync(&spoke);
    (hub, spoke)
}

fn sync(n: &Node) {
    let r = fed::sync(&n.store).unwrap();
    assert!(r.push_rejected.is_empty(), "{:?}", r.push_rejected);
    assert!(r.pull_rejected.is_empty(), "{:?}", r.pull_rejected);
}

fn report(lane: &str, kind: ReportKind) -> LaneReportInput {
    LaneReportInput {
        agent: "codex@shop".into(),
        lane: lane.into(),
        kind,
        summary: "from the laptop".into(),
        progress: None,
        status: None,
        links: vec![],
        artifacts: vec![],
        outcome: None,
        pr_url: None,
    }
}

#[test]
fn a_mirrored_lane_refuses_a_done_report_and_any_update() {
    let (hub, spoke) = pair();
    hub.store
        .create_lane(
            NewLane {
                agent: "manager".into(),
                name: "api".into(),
                project: "shop".into(),
                lead: "claude@shop".into(),
                charter: "charter".into(),
                status: Some(LaneStatus::Working),
            },
            None,
        )
        .unwrap();
    sync(&spoke);
    let mirror = spoke.store.lane("api", None).unwrap().lane;
    assert_eq!(mirror.node.as_deref(), Some("hub"));

    let e = spoke
        .store
        .report_lane(
            LaneReportInput {
                outcome: Some("shipped".into()),
                ..report(&mirror.lane_id, ReportKind::Done)
            },
            None,
        )
        .unwrap_err();
    assert_eq!(e.code(), "conflict");
    assert!(e.to_string().contains("finish it there"), "{e}");

    let e = spoke
        .store
        .update_lane(
            LaneUpdate {
                agent: "codex@shop".into(),
                lane: mirror.lane_id.clone(),
                status: Some(LaneStatus::Blocked),
                ..Default::default()
            },
            None,
        )
        .unwrap_err();
    assert_eq!(e.code(), "conflict");
    assert!(e.to_string().contains("change it there"), "{e}");

    // Neither reached the hub.
    sync(&spoke);
    let lane = hub.store.lane("api", None).unwrap();
    assert_eq!(lane.lane.status, LaneStatus::Working);
    assert!(lane.reports.is_empty(), "{:?}", lane.reports);
}

fn ask(spoke: &Node) -> Decision {
    spoke
        .store
        .ask_decision(
            NewDecision {
                agent: "codex@shop".into(),
                question: "Use the hub's schema?".into(),
                options: ["yes", "no"]
                    .map(|v| DecisionOption {
                        value: v.into(),
                        label: v.into(),
                        description: None,
                    })
                    .to_vec(),
                human: Some("pat".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap()
}

fn relay(hub: &Node, decision: &str, value: &str, by: Option<&str>) -> Result<Decision> {
    hub.store.answer_decision(
        AnswerDecision {
            decision: decision.into(),
            value: Some(value.into()),
            answered_by: by.map(Into::into),
            relayed_by: Some("manager".into()),
            ..Default::default()
        },
        None,
    )
}

fn ignored(s: &Store, decision: &str) -> Vec<String> {
    s.events(10_000)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "decision.answer_ignored" && e.payload["decision_id"] == decision)
        .map(|e| e.payload["status"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn a_relayed_answer_that_arrives_after_the_decision_closed_loses() {
    let (hub, spoke) = pair();
    let d = ask(&spoke);
    sync(&spoke);

    // Answered at home while the relay is on its way.
    spoke
        .store
        .answer_decision(
            AnswerDecision {
                decision: d.decision_id.clone(),
                value: Some("no".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    relay(&hub, &d.decision_id, "yes", None).unwrap();
    sync(&spoke);

    let done = spoke.store.decision(&d.decision_id, None).unwrap();
    assert_eq!(done.answer.unwrap().value.as_deref(), Some("no"));
    assert_eq!(ignored(&spoke.store, &d.decision_id), vec!["answered"]);

    // The feed says what happened.
    let feed = spoke
        .store
        .activity(ActivityQuery {
            since: Some(Since::Time(0)),
            limit: 500,
            kinds: vec![],
        })
        .unwrap();
    assert!(
        feed.items
            .iter()
            .any(|i| i.summary.contains("was ignored (already answered)")),
        "{:?}",
        feed.items.iter().map(|i| &i.summary).collect::<Vec<_>>()
    );
}

/// `answered_by` is a label the relaying manager supplies, not an identity: docs/MANAGER-BOT.md
/// records the bot as `relayed_by` and the name as given. The accountable party is the relayer.
#[test]
fn a_relayed_answer_records_the_name_it_was_given_and_keeps_the_sync_moving() {
    let (hub, spoke) = pair();
    let d = ask(&spoke);
    sync(&spoke);
    relay(&hub, &d.decision_id, "yes", Some("nobody")).unwrap();
    sync(&spoke);
    let answer = spoke
        .store
        .decision(&d.decision_id, None)
        .unwrap()
        .answer
        .unwrap();
    assert_eq!(answer.answered_by, "nobody");
    assert!(answer.relayed_by.unwrap().starts_with("manager"));

    // Later events still arrive.
    hub.store
        .create_lane(
            NewLane {
                agent: "manager".into(),
                name: "after".into(),
                project: "shop".into(),
                lead: "claude@shop".into(),
                charter: "charter".into(),
                status: None,
            },
            None,
        )
        .unwrap();
    sync(&spoke);
    assert_eq!(spoke.store.lane("after", None).unwrap().lane.name, "after");
}
