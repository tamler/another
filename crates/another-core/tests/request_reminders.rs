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
        let store = Store::open_with_clock(&dir.path().join("home"), clock)
            .unwrap()
            .unguarded_repair();
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

    /// Pure inbox projection. Maintenance is an explicit sweep.
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
    fx.sweep();
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
    fx.sweep();
    assert_eq!(fx.mail("other", "Due soon:").len(), 1);
    assert_eq!(fx.kinds(&rid, "due_soon"), 3);
}

/// R1 (review R1 P2-1): one due-soon reminder per (request, owner, deadline) for the life of
/// the request, whatever moves happen in between.
#[test]
fn the_same_deadline_is_never_reminded_twice_to_the_same_owner() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fx::new();
    // Two hours: lead 24 min, so the window opens 96 min in.
    let rid = fx.open(Some(7200));
    fx.act("worker", &rid, RequestAction::Claim, None);
    let d1 = T0 + 7200 * 1000;
    fx.advance_secs(100 * 60);
    fx.sweep();
    assert_eq!(fx.kinds(&rid, "due_soon"), 1);

    // D1 -> D2 -> D1 within the window: D1 was already reminded.
    fx.act("worker", &rid, RequestAction::Claim, Some(7200));
    let now = fx.store.now();
    fx.act(
        "worker",
        &rid,
        RequestAction::Claim,
        Some((d1 - now) / 1000),
    );
    assert_eq!(fx.store.request(&rid).unwrap().due_at, Some(fmt_ts(d1)));
    fx.sweep();
    assert_eq!(fx.kinds(&rid, "due_soon"), 1, "D1 again");

    // A same-value deadline update.
    fx.act(
        "worker",
        &rid,
        RequestAction::Claim,
        Some((d1 - now) / 1000),
    );
    fx.sweep();
    assert_eq!(fx.kinds(&rid, "due_soon"), 1, "same value");

    // Release and reclaim by the same owner keeps the future deadline. The new claim's
    // window (20 min left: lead 5 min) opens; the owner is still reminded only once.
    fx.act("worker", &rid, RequestAction::Release, None);
    fx.act("worker", &rid, RequestAction::Claim, None);
    assert_eq!(fx.store.request(&rid).unwrap().due_at, Some(fmt_ts(d1)));
    fx.advance_secs(16 * 60);
    fx.sweep();
    assert_eq!(fx.kinds(&rid, "due_soon"), 1, "release and reclaim");

    // A different owner taking over with the same deadline is reminded in its own right,
    // with the lead computed from its own claim (20 min left: lead 5 min).
    let fx = Fx::new();
    let rid = fx.open(Some(7200));
    fx.act("worker", &rid, RequestAction::Claim, None);
    fx.advance_secs(100 * 60);
    fx.sweep();
    assert_eq!(fx.kinds(&rid, "due_soon"), 1);
    fx.act("worker", &rid, RequestAction::Release, None);
    fx.act("other", &rid, RequestAction::Claim, None);
    fx.sweep();
    assert_eq!(
        fx.kinds(&rid, "due_soon"),
        1,
        "not yet in the new owner's window"
    );
    fx.advance_secs(16 * 60);
    fx.sweep();
    assert_eq!(fx.kinds(&rid, "due_soon"), 2);
    assert_eq!(fx.mail("other", "Due soon:").len(), 1);
}

/// Review R2 P2: a thread can hold several requests. Two with the same requester, owner,
/// title and deadline in one thread are still two reminders.
#[test]
fn two_requests_in_one_thread_each_get_a_reminder() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fx::new();
    let a = fx.open(Some(3600));
    let thread = fx.store.request(&a).unwrap().thread_id;
    let b = fx
        .store
        .send_message(SendMessage {
            from: "lead".into(),
            to: vec!["worker".into()],
            subject: "Fix the flaky test".into(),
            body: "the other half".into(),
            thread_id: Some(thread.clone()),
            request: Some(NewRequest {
                due_in_seconds: Some(3600),
            }),
            ..Default::default()
        })
        .unwrap()
        .request_id
        .expect("second request");
    assert_eq!(fx.store.request(&b).unwrap().thread_id, thread);
    fx.act("worker", &a, RequestAction::Claim, None);
    fx.act("worker", &b, RequestAction::Claim, None);
    fx.advance_secs(49 * 60);
    fx.sweep();
    assert_eq!(fx.kinds(&a, "due_soon"), 1);
    assert_eq!(fx.kinds(&b, "due_soon"), 1);
    fx.sweep();
    assert_eq!(fx.mail("worker", "Due soon:").len(), 2);
}

#[test]
fn the_window_edges_are_exact() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    // Exactly five minutes left at the claim: eligible, and the window opens at once.
    let fx = Fx::new();
    let rid = fx.open(Some(300));
    fx.act("worker", &rid, RequestAction::Claim, None);
    fx.sweep();
    assert_eq!(fx.kinds(&rid, "due_soon"), 1);

    // 299 seconds: never.
    let fx = Fx::new();
    let rid = fx.open(Some(299));
    fx.act("worker", &rid, RequestAction::Claim, None);
    fx.advance_secs(298);
    fx.sweep();
    assert_eq!(fx.kinds(&rid, "due_soon"), 0);

    // A one-hour claim: the window opens exactly 12 minutes before the deadline.
    let fx = Fx::new();
    let rid = fx.open(Some(3600));
    fx.act("worker", &rid, RequestAction::Claim, None);
    fx.advance_secs(48 * 60 - 1);
    fx.sweep();
    assert_eq!(
        fx.kinds(&rid, "due_soon"),
        0,
        "one second before the window"
    );
    fx.advance_secs(1);
    fx.sweep();
    assert_eq!(fx.kinds(&rid, "due_soon"), 1, "at the window's start");

    // One second before the deadline is still in time; at the deadline it is overdue.
    let fx = Fx::new();
    let rid = fx.open(Some(3600));
    fx.act("worker", &rid, RequestAction::Claim, None);
    fx.advance_secs(3599);
    fx.sweep();
    assert_eq!(fx.kinds(&rid, "due_soon"), 1);
    let fx = Fx::new();
    let rid = fx.open(Some(3600));
    fx.act("worker", &rid, RequestAction::Claim, None);
    fx.advance_secs(3600);
    fx.sweep();
    assert_eq!(fx.kinds(&rid, "due_soon"), 0);
    assert_eq!(fx.kinds(&rid, "overdue"), 1);
}

/// Review R1 P2-3: reminders run in the full sweep, not on every write. Sending mail does
/// not sweep; reading an inbox does not either.
#[test]
fn only_a_sweep_sends_the_due_soon_reminder() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fx::new();
    let rid = fx.open(Some(3600));
    fx.act("worker", &rid, RequestAction::Claim, None);
    fx.advance_secs(50 * 60);
    fx.open_from("other", "lead", None);
    fx.store
        .send_message(SendMessage {
            from: "worker".into(),
            to: vec!["lead".into()],
            subject: "progress".into(),
            body: "halfway".into(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(fx.kinds(&rid, "due_soon"), 0, "sending does not sweep");
    assert_eq!(
        fx.mail("worker", "Due soon:").len(),
        0,
        "the inbox read is pure"
    );
    assert_eq!(fx.kinds(&rid, "due_soon"), 0);
    fx.sweep();
    assert_eq!(fx.mail("worker", "Due soon:").len(), 1);
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
    fx.sweep();
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
    let requests_before = fx.store.requests(RequestFilter::default()).unwrap().len();
    fx.advance_secs(601);
    fx.sweep();
    let got = fx.mail("lead", "Overdue:");
    assert_eq!(got.len(), 1);
    assert!(
        got[0].body.contains("nobody claiming it"),
        "{}",
        got[0].body
    );
    // A notice from the requester to itself says it is automatic, and opens no request.
    assert!(
        got[0].body.ends_with("(Automatic notice from Another.)"),
        "{}",
        got[0].body
    );
    assert_eq!(got[0].request_id, None);
    assert_eq!(
        fx.store.requests(RequestFilter::default()).unwrap().len(),
        requests_before
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

/// The dashboard's own past-due pass is a natural transition and still notifies; only the
/// request the person marked is silent.
#[test]
fn a_dashboard_mark_still_notifies_other_overdue_requests() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    let fx = Fx::new();
    let overdue = fx.open(Some(600));
    fx.act("worker", &overdue, RequestAction::Claim, None);
    let marked = fx.open(Some(7200));
    fx.act("worker", &marked, RequestAction::Claim, None);
    fx.advance_secs(601);
    fx.store.expire_request_from_dashboard(&marked).unwrap();
    assert_eq!(fx.kinds(&overdue, "overdue"), 1);
    assert!(fx.reminders(&marked).is_empty());
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
    // No partial message was left behind for the failed notice.
    let thread = fx.store.request(&failing).unwrap().thread_id;
    let leftovers: i64 = rusqlite::Connection::open(fx.dir.path().join("home").join("another.db"))
        .unwrap()
        .query_row(
            "SELECT count(*) FROM messages WHERE thread_id = ?1 AND subject LIKE 'Overdue:%'",
            [&thread],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(leftovers, 0);
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

    // The setting is per process and read at each sweep: a sweep without it, still inside
    // the window, sends the reminder the opted-out sweep skipped.
    let fx = Fx::new();
    let rid = fx.open(Some(3600));
    fx.act("worker", &rid, RequestAction::Claim, None);
    fx.advance_secs(50 * 60);
    unsafe { std::env::set_var("ANOTHER_REQUEST_REMINDERS", "OFF") };
    fx.sweep();
    let skipped = fx.kinds(&rid, "due_soon");
    unsafe { std::env::remove_var("ANOTHER_REQUEST_REMINDERS") };
    assert_eq!(skipped, 0);
    fx.sweep();
    assert_eq!(fx.kinds(&rid, "due_soon"), 1);
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
