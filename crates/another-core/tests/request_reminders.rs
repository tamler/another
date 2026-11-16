//! Request deadline reminders: a due-soon reminder to the owner before the deadline and an
//! overdue notice to the requester when the sweep marks the request stale.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use another_core::*;

const T0: i64 = 1_800_000_000_000;

/// The opt-out is a process environment variable; tests that touch it, or depend on it being
/// unset, take this lock.
static ENV: Mutex<()> = Mutex::new(());

struct Fx {
    dir: tempfile::TempDir,
    store: Store,
    clock: Arc<AtomicI64>,
}

impl Fx {
    fn new() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work/app").to_string_lossy().into_owned();
        let (clock, handle) = Clock::manual(T0);
        let store = Store::open_with_clock(&dir.path().join("home"), clock).unwrap();
        store.ensure_project("test", &path).unwrap();
        for name in ["lead", "worker", "other"] {
            store
                .register_agent(RegisterAgent {
                    name: name.into(),
                    program: Program::Claude,
                    project: None,
                    model: None,
                    description: None,
                })
                .unwrap();
        }
        Fx {
            dir,
            store,
            clock: handle,
        }
    }

    fn advance_secs(&self, secs: i64) {
        self.clock.fetch_add(secs * 1000, Ordering::SeqCst);
    }

    /// `from` opens a request to `to`, due in `due` seconds.
    fn open_from(&self, from: &str, to: &str, due: Option<i64>) -> String {
        self.store
            .send_message(SendMessage {
                from: from.into(),
                to: vec![to.into()],
                subject: "Fix the flaky test".into(),
                body: "tests/net.rs".into(),
                request: Some(NewRequest {
                    due_in_seconds: due,
                }),
                ..Default::default()
            })
            .unwrap()
            .request_id
            .expect("request opened")
    }

    fn open(&self, due: Option<i64>) -> String {
        self.open_from("lead", "worker", due)
    }

    fn act(&self, agent: &str, rid: &str, action: RequestAction, due: Option<i64>) -> Request {
        self.store
            .update_request(RequestUpdate {
                agent: agent.into(),
                request_id: rid.into(),
                action,
                note: None,
                due_in_seconds: due,
            })
            .unwrap()
    }

    fn sweep(&self) {
        self.store.sweep().unwrap();
    }

    fn reminders(&self, rid: &str) -> Vec<serde_json::Value> {
        self.store
            .events(10_000)
            .unwrap()
            .into_iter()
            .filter(|e| {
                e.kind == "request.reminded"
                    && e.payload.get("request_id").and_then(|v| v.as_str()) == Some(rid)
            })
            .map(|e| e.payload)
            .collect()
    }

    fn kinds(&self, rid: &str, kind: &str) -> usize {
        self.reminders(rid)
            .iter()
            .filter(|p| p["kind"] == kind)
            .count()
    }

    /// Messages in `who`'s inbox whose subject starts with `prefix`. Reading the inbox
    /// sweeps, as every hook does.
    fn mail(&self, who: &str, prefix: &str) -> Vec<Message> {
        self.store
            .inbox(
                who,
                InboxQuery {
                    unread_only: false,
                    urgent_only: false,
                    limit: 100,
                },
            )
            .unwrap()
            .messages
            .into_iter()
            .map(|i| i.message)
            .filter(|m| m.subject.starts_with(prefix))
            .collect()
    }

    fn address(&self, name: &str) -> String {
        self.store
            .agents()
            .unwrap()
            .into_iter()
            .find(|a| a.name == name)
            .unwrap()
            .address
    }
}

#[test]
fn the_owner_is_reminded_once_shortly_before_the_deadline() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fx::new();
    let rid = fx.open(Some(3600));
    fx.act("worker", &rid, RequestAction::Claim, None);
    let request_mid = fx.store.request(&rid).unwrap().message_id;

    // Lead is a fifth of the claimed span: 12 minutes before a 60-minute deadline.
    fx.advance_secs(47 * 60);
    fx.sweep();
    assert_eq!(fx.kinds(&rid, "due_soon"), 0, "47 min in is too early");
    assert!(fx.mail("worker", "Due soon:").is_empty());

    fx.advance_secs(2 * 60);
    let got = fx.mail("worker", "Due soon:");
    assert_eq!(got.len(), 1, "one reminder 49 min in");
    let m = &got[0];
    assert_eq!(m.subject, "Due soon: Fix the flaky test");
    assert_eq!(m.from, fx.address("lead"), "it comes from the requester");
    assert_eq!(
        m.reply_to.as_deref(),
        Some(request_mid.as_str()),
        "in the request thread"
    );
    assert!(
        m.body.contains(&format!("another request complete {rid}")),
        "{}",
        m.body
    );
    assert!(
        m.body.contains(&format!("another request release {rid}")),
        "{}",
        m.body
    );
    assert!(m.body.contains("in 11 min"), "{}", m.body);
    let ev = &fx.reminders(&rid)[0];
    assert_eq!(ev["kind"], "due_soon");
    assert_eq!(ev["message_id"], m.message_id.as_str());

    fx.advance_secs(60);
    fx.sweep();
    fx.sweep();
    assert_eq!(
        fx.kinds(&rid, "due_soon"),
        1,
        "never twice for one deadline"
    );
    assert_eq!(fx.mail("worker", "Due soon:").len(), 1);
    // The requester does not get a copy.
    assert!(fx.mail("lead", "Due soon:").is_empty());
}

#[test]
fn the_lead_is_clamped_between_five_minutes_and_an_hour() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    // 10-minute span: a fifth is 2 min, clamped up to 5 min.
    let fx = Fx::new();
    let rid = fx.open(Some(600));
    fx.act("worker", &rid, RequestAction::Claim, None);
    fx.advance_secs(4 * 60 + 59);
    fx.sweep();
    assert_eq!(fx.kinds(&rid, "due_soon"), 0);
    fx.advance_secs(2);
    fx.sweep();
    assert_eq!(fx.kinds(&rid, "due_soon"), 1);

    // One-day span: a fifth is 4.8 h, clamped down to 1 h.
    let fx = Fx::new();
    let rid = fx.open(Some(86_400));
    fx.act("worker", &rid, RequestAction::Claim, None);
    fx.advance_secs(86_400 - 3600 - 1);
    fx.sweep();
    assert_eq!(fx.kinds(&rid, "due_soon"), 0);
    fx.advance_secs(2);
    fx.sweep();
    assert_eq!(fx.kinds(&rid, "due_soon"), 1);

    // Claimed with under five minutes to go: no due-soon reminder at all.
    let fx = Fx::new();
    let rid = fx.open(Some(200));
    fx.act("worker", &rid, RequestAction::Claim, None);
    for _ in 0..19 {
        fx.advance_secs(10);
        fx.sweep();
    }
    assert_eq!(fx.kinds(&rid, "due_soon"), 0);
}

#[test]
fn a_new_deadline_or_a_takeover_rearms_the_reminder() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fx::new();
    let rid = fx.open(Some(3600));
    fx.act("worker", &rid, RequestAction::Claim, None);
    fx.advance_secs(50 * 60);
    fx.sweep();
    assert_eq!(fx.kinds(&rid, "due_soon"), 1);

    // The owner moves the deadline an hour out: a new deadline, a new reminder.
    fx.act("worker", &rid, RequestAction::Claim, Some(3600));
    fx.sweep();
    assert_eq!(fx.kinds(&rid, "due_soon"), 1, "not yet in the new window");
    fx.advance_secs(55 * 60);
    fx.sweep();
    assert_eq!(fx.kinds(&rid, "due_soon"), 2);

    // It goes stale, `other` takes it over with a new deadline: reminded in turn.
    fx.advance_secs(10 * 60);
    fx.sweep();
    assert_eq!(fx.store.request(&rid).unwrap().status, RequestStatus::Stale);
    fx.act("other", &rid, RequestAction::Claim, Some(1800));
    fx.advance_secs(25 * 60);
    assert_eq!(fx.mail("other", "Due soon:").len(), 1);
    assert_eq!(fx.kinds(&rid, "due_soon"), 3);
}

#[test]
fn a_missed_window_sends_only_the_overdue_notice() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fx::new();
    let rid = fx.open(Some(3600));
    fx.act("worker", &rid, RequestAction::Claim, None);
    // Nothing sweeps (no daemon, no writes) until the deadline has passed.
    fx.advance_secs(3600 + 60);
    fx.sweep();
    assert_eq!(fx.store.request(&rid).unwrap().status, RequestStatus::Stale);
    assert_eq!(
        fx.kinds(&rid, "due_soon"),
        0,
        "no due-soon after the deadline"
    );
    assert_eq!(fx.kinds(&rid, "overdue"), 1);
    assert!(fx.mail("worker", "Due soon:").is_empty());
}

#[test]
fn the_requester_is_told_once_when_a_claimed_request_goes_overdue() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fx::new();
    let rid = fx.open(Some(600));
    fx.act("worker", &rid, RequestAction::Claim, None);
    let request_mid = fx.store.request(&rid).unwrap().message_id;
    fx.advance_secs(601);
    let got = fx.mail("lead", "Overdue:");
    assert_eq!(got.len(), 1);
    let m = &got[0];
    assert_eq!(m.subject, "Overdue: Fix the flaky test");
    assert_eq!(m.from, fx.address("worker"), "it comes from the owner");
    assert_eq!(m.reply_to.as_deref(), Some(request_mid.as_str()));
    assert!(m.body.contains(&fx.address("worker")), "{}", m.body);
    assert!(
        m.body.contains(&format!("another request claim {rid}")),
        "{}",
        m.body
    );
    assert!(
        m.body.contains(&format!("another request cancel {rid}")),
        "{}",
        m.body
    );
    fx.advance_secs(3600);
    fx.sweep();
    assert_eq!(fx.mail("lead", "Overdue:").len(), 1, "once per transition");
    assert_eq!(fx.kinds(&rid, "overdue"), 1);
    // The owner is not sent the overdue notice.
    assert!(fx.mail("worker", "Overdue:").is_empty());
}

#[test]
fn an_unclaimed_overdue_request_tells_only_the_requester() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fx::new();
    let rid = fx.open(Some(600));
    fx.advance_secs(601);
    let got = fx.mail("lead", "Overdue:");
    assert_eq!(got.len(), 1);
    assert!(
        got[0].body.contains("nobody claiming it"),
        "{}",
        got[0].body
    );
    assert!(
        fx.mail("worker", "Overdue:").is_empty(),
        "the addressee is not told"
    );
    assert_eq!(fx.kinds(&rid, "overdue"), 1);
}

#[test]
fn work_one_requested_of_itself_gets_no_reminders() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fx::new();
    // `lead` asks `worker`, but `lead` claims it itself: requester and owner are one agent.
    let rid = fx.open(Some(3600));
    fx.act("lead", &rid, RequestAction::Claim, None);
    fx.advance_secs(50 * 60);
    fx.sweep();
    fx.advance_secs(20 * 60);
    fx.sweep();
    assert_eq!(fx.store.request(&rid).unwrap().status, RequestStatus::Stale);
    assert!(fx.reminders(&rid).is_empty(), "{:?}", fx.reminders(&rid));
}

#[test]
fn a_dashboard_mark_stale_sends_no_notice() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fx::new();
    let rid = fx.open(Some(3600));
    fx.act("worker", &rid, RequestAction::Claim, None);
    fx.store.expire_request_from_dashboard(&rid).unwrap();
    fx.advance_secs(2 * 3600);
    fx.sweep();
    assert!(fx.reminders(&rid).is_empty(), "{:?}", fx.reminders(&rid));
}

#[test]
fn closed_or_released_requests_get_no_reminders() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fx::new();
    let done = fx.open(Some(3600));
    fx.act("worker", &done, RequestAction::Claim, None);
    fx.act("worker", &done, RequestAction::Complete, None);
    let cancelled = fx.open(Some(3600));
    fx.act("worker", &cancelled, RequestAction::Claim, None);
    fx.act("lead", &cancelled, RequestAction::Cancel, None);
    let released = fx.open(Some(3600));
    fx.act("worker", &released, RequestAction::Claim, None);
    fx.act("worker", &released, RequestAction::Release, None);
    fx.advance_secs(50 * 60);
    fx.sweep();
    for rid in [&done, &cancelled, &released] {
        assert_eq!(fx.kinds(rid, "due_soon"), 0, "{rid}");
    }
    fx.advance_secs(20 * 60);
    fx.sweep();
    assert!(fx.reminders(&done).is_empty());
    assert!(fx.reminders(&cancelled).is_empty());
    // The released one is open again and past due: the requester is told, as for any
    // unclaimed request.
    assert_eq!(fx.kinds(&released, "due_soon"), 0);
    assert_eq!(fx.kinds(&released, "overdue"), 1);
}

#[test]
fn a_failed_notice_never_blocks_the_sweep() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fx::new();
    let failing = fx.open(Some(600));
    fx.act("worker", &failing, RequestAction::Claim, None);
    let fine = fx.open_from("other", "worker", Some(600));
    fx.store
        .acquire_holds(AcquireHolds {
            agent: "worker".into(),
            project: fx
                .dir
                .path()
                .join("work/app")
                .to_string_lossy()
                .into_owned(),
            paths: vec!["src/a.rs".into()],
            exclusive: true,
            ttl_seconds: Some(300),
            reason: None,
            repo_wide: false,
        })
        .unwrap();
    // Any delivery to the failing request's requester is refused.
    let db = rusqlite::Connection::open(fx.dir.path().join("home").join("another.db")).unwrap();
    let lead_id: String = db
        .query_row("SELECT agent_id FROM agents WHERE name = 'lead'", [], |r| {
            r.get(0)
        })
        .unwrap();
    db.execute_batch(&format!(
        "CREATE TRIGGER refuse_notice BEFORE INSERT ON deliveries
         WHEN NEW.agent_id = '{lead_id}'
         BEGIN SELECT RAISE(ABORT, 'refused by test'); END;"
    ))
    .unwrap();
    drop(db);

    fx.advance_secs(601);
    let report = fx.store.sweep().unwrap();
    assert_eq!(report.stale_requests, 2);
    assert_eq!(report.expired_holds, 1, "the rest of the sweep ran");
    assert_eq!(
        fx.store.request(&failing).unwrap().status,
        RequestStatus::Stale
    );
    assert_eq!(
        fx.store.request(&fine).unwrap().status,
        RequestStatus::Stale
    );
    assert_eq!(
        fx.kinds(&failing, "overdue"),
        0,
        "the failed reminder was rolled back"
    );
    assert_eq!(fx.kinds(&fine, "overdue"), 1);
    let failed: Vec<_> = fx
        .store
        .events(10_000)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "request.notify_failed")
        .collect();
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert_eq!(failed[0].payload["request_id"], failing.as_str());
    assert_eq!(failed[0].payload["kind"], "overdue");
}

#[test]
fn the_environment_opt_out_stops_reminders_but_not_staleness() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: every test in this file that reads the variable holds `ENV`.
    unsafe { std::env::set_var("ANOTHER_REQUEST_REMINDERS", "off") };
    let fx = Fx::new();
    let rid = fx.open(Some(3600));
    fx.act("worker", &rid, RequestAction::Claim, None);
    fx.advance_secs(50 * 60);
    fx.sweep();
    fx.advance_secs(20 * 60);
    fx.sweep();
    let status = fx.store.request(&rid).unwrap().status;
    let reminders = fx.reminders(&rid);
    unsafe { std::env::remove_var("ANOTHER_REQUEST_REMINDERS") };
    assert_eq!(status, RequestStatus::Stale);
    assert!(reminders.is_empty(), "{reminders:?}");
}

#[test]
fn a_reminder_is_announced_once_and_shows_in_activity() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fx::new();
    let rid = fx.open(Some(3600));
    fx.act("worker", &rid, RequestAction::Claim, None);
    fx.advance_secs(50 * 60);
    fx.sweep();
    let feed = fx
        .store
        .activity(ActivityQuery {
            since: None,
            limit: 100,
            kinds: vec![],
        })
        .unwrap();
    let summaries: Vec<_> = feed.items.iter().map(|i| i.summary.clone()).collect();
    assert!(
        summaries
            .iter()
            .any(|s| s.contains("reminded") && s.contains("due soon: Fix the flaky test")),
        "{summaries:?}"
    );
    let unread = fx
        .store
        .inbox(
            "worker",
            InboxQuery {
                unread_only: true,
                urgent_only: false,
                limit: 100,
            },
        )
        .unwrap();
    assert_eq!(
        unread
            .messages
            .iter()
            .filter(|i| i.message.subject.starts_with("Due soon:"))
            .count(),
        1
    );
}
