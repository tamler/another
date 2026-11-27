//! The work queue for one node (scope id M1, with C2 and C3): admission, the atomic `next`,
//! limits, results, handoff, admin release and its notice, the owner list and the audit.
//! Test names and numbers follow the queue spec's test plan.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Barrier};

use another_core::*;
use rusqlite::Connection;
use serde_json::{json, Value};

const T0: i64 = 1_800_000_000_000;
const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;

fn owner() -> QueueCaller {
    QueueCaller::Owner
}

fn agent(name: &str) -> QueueCaller {
    QueueCaller::Agent {
        agent: name.into(),
        admit_key: None,
    }
}

fn register(store: &Store, name: &str) -> String {
    store
        .register_agent(RegisterAgent {
            name: name.into(),
            program: Program::Claude,
            project: None,
            model: None,
            description: None,
        })
        .unwrap()
        .agent_id
}

struct Fx {
    dir: tempfile::TempDir,
    store: Store,
    clock: Arc<AtomicI64>,
}

impl Fx {
    fn new() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let (clock, handle) = Clock::manual(T0);
        let store = Store::open_with_clock(&dir.path().join("home"), clock).unwrap();
        for name in ["ann", "bob", "cy", "arch"] {
            register(&store, name);
        }
        Fx {
            dir,
            store,
            clock: handle,
        }
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn db(&self) -> Connection {
        Connection::open(self.home().join(DB_FILE)).unwrap()
    }

    fn advance(&self, ms: i64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }

    fn agent_id(&self, name: &str) -> String {
        self.store.agent(name).unwrap().agent_id
    }

    /// A proposal by `who` with a scope and a done condition, so it can be admitted as is.
    fn propose(&self, who: &str, title: &str) -> QueueItem {
        self.store
            .queue_propose(
                &agent(who),
                ProposeItem {
                    title: title.into(),
                    scope: Some("M1".into()),
                    done_when: Some("the tests pass".into()),
                    ..Default::default()
                },
            )
            .unwrap()
    }

    /// An item `ann` proposed and the owner admitted.
    fn ready(&self, title: &str) -> QueueItem {
        let item = self.propose("ann", title);
        self.store
            .queue_admit(&owner(), &item.label, AdmitItem::default())
            .unwrap()
    }

    fn claim(&self, who: &str) -> QueueItem {
        self.store
            .queue_next(who)
            .unwrap()
            .claimed
            .unwrap_or_else(|| panic!("{who} was handed nothing"))
    }

    fn item(&self, item: &str) -> QueueItem {
        self.store.queue_item(item).unwrap()
    }

    fn events(&self, kind: &str) -> Vec<Event> {
        self.store
            .events(100_000)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == kind)
            .collect()
    }

    /// Make `arch` a delegate and return it as a caller with its key.
    fn delegate(&self) -> QueueCaller {
        let made = self.store.queue_delegate_add(&owner(), "arch").unwrap();
        QueueCaller::Agent {
            agent: "arch".into(),
            admit_key: Some(made.key),
        }
    }

    /// Every queue row and history row, as text: equal snapshots mean nothing changed.
    fn snapshot(&self) -> String {
        let db = self.db();
        let mut out = String::new();
        for sql in [
            "SELECT * FROM queue_items ORDER BY seq",
            "SELECT * FROM queue_history ORDER BY rowid",
            "SELECT * FROM queue_limits ORDER BY agent_id",
            "SELECT agent_id, key_hash, removed_at FROM queue_delegates ORDER BY agent_id",
            "SELECT * FROM queue_settings ORDER BY key",
        ] {
            let mut stmt = db.prepare(sql).unwrap();
            let n = stmt.column_count();
            let mut rows = stmt.query([]).unwrap();
            while let Some(r) = rows.next().unwrap() {
                for i in 0..n {
                    out.push_str(&format!(
                        "{:?}|",
                        r.get::<_, rusqlite::types::Value>(i).unwrap()
                    ));
                }
                out.push('\n');
            }
        }
        out
    }

    fn unread(&self, who: &str) -> Vec<InboxItem> {
        self.store
            .inbox(
                who,
                InboxQuery {
                    unread_only: true,
                    urgent_only: false,
                    limit: 50,
                },
            )
            .unwrap()
            .messages
    }
}

fn code<T: std::fmt::Debug>(r: Result<T>) -> &'static str {
    r.expect_err("expected an error").code()
}

fn done(pointer: &str) -> DoneItem {
    DoneItem {
        summary: "merged".into(),
        pointer_kind: PointerKind::Commit,
        pointer: pointer.into(),
    }
}

/// Run `f` until it does not fail on a busy database.
fn retry<T>(mut f: impl FnMut() -> Result<T>) -> T {
    loop {
        match f() {
            Ok(v) => return v,
            Err(Error::Db(_)) => std::thread::sleep(std::time::Duration::from_millis(5)),
            Err(e) => panic!("{e}"),
        }
    }
}

/// A home with fifty agents and `items` ready items, and one store handle per agent.
fn racing_home(items: usize) -> (tempfile::TempDir, Store, Vec<(String, Store)>) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let store = Store::open(&home).unwrap();
    let names: Vec<String> = (0..50).map(|i| format!("w{i:02}")).collect();
    for name in &names {
        register(&store, name);
    }
    for i in 0..items {
        let item = store
            .queue_propose(
                &owner(),
                ProposeItem {
                    title: format!("item {i}"),
                    scope: Some("M1".into()),
                    done_when: Some("done".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        store
            .queue_admit(&owner(), &item.label, AdmitItem::default())
            .unwrap();
    }
    let handles = names
        .into_iter()
        .map(|n| (n, Store::open(&home).unwrap()))
        .collect();
    (dir, store, handles)
}

// T1
#[test]
fn fifty_agents_racing_for_next_never_share_an_item() {
    // Twenty items, fifty agents asking once each at the same moment.
    let (_dir, store, handles) = racing_home(20);
    let barrier = Arc::new(Barrier::new(handles.len()));
    let threads: Vec<_> = handles
        .into_iter()
        .map(|(name, s)| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                retry(|| s.queue_next(&name))
            })
        })
        .collect();
    let outcomes: Vec<QueueNext> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    let claimed: Vec<String> = outcomes
        .iter()
        .filter_map(|o| o.claimed.as_ref().map(|i| i.item_id.clone()))
        .collect();
    assert_eq!(claimed.len(), 20, "every item was handed out");
    assert_eq!(
        claimed.iter().collect::<BTreeSet<_>>().len(),
        20,
        "no item was handed out twice"
    );
    let empty = outcomes
        .iter()
        .filter(|o| o.claimed.is_none() && o.refused.is_none())
        .count();
    assert_eq!(
        empty, 30,
        "the other thirty agents were told nothing is ready"
    );
    let audit = store.queue_audit(None).unwrap();
    assert!(audit.ok, "{:?}", audit.findings);
    assert_eq!(audit.claims, 20);

    // Two hundred items, each agent looping through `next` and `done` until nothing is left.
    let (_dir, store, handles) = racing_home(200);
    let barrier = Arc::new(Barrier::new(handles.len()));
    let threads: Vec<_> = handles
        .into_iter()
        .map(|(name, s)| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let mut finished = Vec::new();
                while let Some(item) = retry(|| s.queue_next(&name)).claimed {
                    retry(|| s.queue_done(&name, &item.item_id, done("abc1234")));
                    finished.push(item.item_id);
                }
                finished
            })
        })
        .collect();
    let finished: Vec<String> = threads
        .into_iter()
        .flat_map(|t| t.join().unwrap())
        .collect();
    assert_eq!(finished.len(), 200);
    assert_eq!(finished.iter().collect::<BTreeSet<_>>().len(), 200);
    let left = store
        .queue_list(QueueFilter {
            limit: 1000,
            ..Default::default()
        })
        .unwrap();
    assert!(left.is_empty(), "nothing is left ready or claimed");
    let audit = store.queue_audit(None).unwrap();
    assert!(audit.ok, "{:?}", audit.findings);
    assert_eq!(
        (audit.items, audit.claims, audit.completions),
        (200, 200, 200)
    );
    // The record is the event log: one claim and one completion per item.
    let events = store.events(100_000).unwrap();
    for kind in ["queue.claimed", "queue.completed"] {
        let ids: Vec<&str> = events
            .iter()
            .filter(|e| e.kind == kind)
            .map(|e| e.payload["item_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 200, "{kind}");
        assert_eq!(ids.iter().collect::<BTreeSet<_>>().len(), 200, "{kind}");
    }
}

/// A small deterministic generator, so a failing sequence can be replayed from its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn pick<'a>(&mut self, of: &[&'a str]) -> &'a str {
        of[self.below(of.len() as u64) as usize]
    }
}

// T3
#[test]
fn a_proposed_item_is_never_handed_out() {
    let agents = ["ann", "bob", "cy"];
    for seed in 1..=500u64 {
        let (clock, _) = Clock::manual(T0);
        let store = Store::open_in_memory(clock).unwrap();
        for name in agents {
            register(&store, name);
        }
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let mut labels: Vec<String> = Vec::new();
        for _ in 0..30 {
            let who = rng.pick(&agents);
            let other = rng.pick(&agents);
            // An ordinary agent has no key; some of them try a made-up one.
            let caller = QueueCaller::Agent {
                agent: who.into(),
                admit_key: (rng.below(2) == 0).then(|| format!("adk_{:064x}", rng.next())),
            };
            let item = if labels.is_empty() {
                "Q1".to_string()
            } else {
                labels[rng.below(labels.len() as u64) as usize].clone()
            };
            match rng.below(12) {
                0..=2 => {
                    let kind = [QueueKind::Work, QueueKind::Security, QueueKind::DataLoss]
                        [rng.below(3) as usize];
                    let full = rng.below(2) == 0;
                    let made = store
                        .queue_propose(
                            &caller,
                            ProposeItem {
                                title: format!("seed {seed}"),
                                scope: full.then(|| "M1".to_string()),
                                done_when: full.then(|| "done".to_string()),
                                kind,
                                ..Default::default()
                            },
                        )
                        .unwrap();
                    labels.push(made.label);
                }
                3 => {
                    let out = store.queue_next(who).unwrap();
                    assert!(
                        out.claimed.is_none(),
                        "seed {seed}: {who} was handed an item"
                    );
                }
                4 => {
                    let _ = store.queue_note(who, &item, "a note");
                }
                5 => {
                    let _ = store.queue_release(
                        &caller,
                        &item,
                        ReleaseItem {
                            reason: (rng.below(2) == 0).then(|| "taking it back".to_string()),
                            ..Default::default()
                        },
                    );
                }
                6 => {
                    let _ = store.queue_release(
                        &caller,
                        &item,
                        ReleaseItem {
                            to: Some(other.to_string()),
                            ..Default::default()
                        },
                    );
                }
                7 => {
                    let _ = store.queue_done(who, &item, done("abc1234"));
                }
                8 => {
                    let _ = store.queue_admit(
                        &caller,
                        &item,
                        AdmitItem {
                            scope: Some("M1".into()),
                            done_when: Some("done".into()),
                            ..Default::default()
                        },
                    );
                }
                9 => {
                    let _ = store.queue_reject(&caller, &item, "no");
                }
                10 => {
                    let _ = store.queue_edit(
                        &caller,
                        &item,
                        EditItem {
                            scope: Some("M2".into()),
                            for_agent: Some(other.to_string()),
                            ..Default::default()
                        },
                    );
                }
                _ => {
                    let _ = store.queue_set_limit(&caller, other, 5);
                    let _ = store.queue_delegate_add(&caller, who);
                    let _ = store.queue_set_grace(&caller, 7200);
                }
            }
        }
        let events = store.events(100_000).unwrap();
        assert!(
            !events
                .iter()
                .any(|e| e.kind == "queue.admitted" || e.kind == "queue.claimed"),
            "seed {seed}: an ordinary agent admitted or claimed an item"
        );
        for state in [QueueState::Ready, QueueState::Claimed] {
            let found = store
                .queue_list(QueueFilter {
                    state: Some(state),
                    limit: 1000,
                    ..Default::default()
                })
                .unwrap();
            assert!(found.is_empty(), "seed {seed}: an item became {state:?}");
        }
    }
}

// T6
#[test]
fn the_table_refuses_a_ready_or_claimed_row_without_approval() {
    let fx = Fx::new();
    let proposed = fx.propose("ann", "not admitted");
    let ann = fx.agent_id("ann");
    let db = fx.db();
    let refused = |sql: &str, what: &str| {
        let err = db
            .execute(sql, rusqlite::params![proposed.seq, ann])
            .expect_err(what);
        assert!(err.to_string().contains("CHECK"), "{what}: {err}");
    };
    refused(
        "UPDATE queue_items SET state = 'ready' WHERE seq = ?1 AND ?2 = ?2",
        "an unadmitted ready row",
    );
    refused(
        "UPDATE queue_items SET state = 'claimed', claimer_id = ?2, claimed_at = 1 WHERE seq = ?1",
        "an unadmitted claimed row",
    );
    refused(
        "UPDATE queue_items SET claimer_id = ?2 WHERE seq = ?1",
        "a claimer on an unclaimed row",
    );
    refused(
        "UPDATE queue_items SET admit_authority = 'delegate', admitted_at = 1 WHERE seq = ?1 AND ?2 = ?2",
        "delegate authority with no admitted_by",
    );
    refused(
        "UPDATE queue_items SET admit_authority = 'owner', admitted_by = ?2, admitted_at = 1 WHERE seq = ?1",
        "owner authority with an admitted_by",
    );
    refused(
        "UPDATE queue_items SET state = 'ready', admit_authority = 'owner', admitted_at = 1,
           scope = NULL WHERE seq = ?1 AND ?2 = ?2",
        "a ready row with no scope",
    );
    // A fast-lane kind on a row a delegate admitted.
    let by_delegate = fx.propose("ann", "admitted by the delegate");
    let arch = fx.delegate();
    fx.store
        .queue_admit(&arch, &by_delegate.label, AdmitItem::default())
        .unwrap();
    let err = db
        .execute(
            "UPDATE queue_items SET kind = 'security' WHERE seq = ?1",
            [by_delegate.seq],
        )
        .expect_err("a fast-lane kind on a delegate-admitted row");
    assert!(err.to_string().contains("CHECK"), "{err}");
    // The rows the code writes pass: the whole approval at once.
    db.execute(
        "UPDATE queue_items SET state = 'ready', admit_authority = 'owner', admitted_at = 1
         WHERE seq = ?1",
        [proposed.seq],
    )
    .unwrap();
}

// T7
#[test]
fn an_admitted_item_shows_who_admitted_it_and_when() {
    let fx = Fx::new();
    // Admit needs a scope and a done condition, from the item or from the command.
    let bare = fx
        .store
        .queue_propose(
            &agent("ann"),
            ProposeItem {
                title: "bare".into(),
                ..Default::default()
            },
        )
        .unwrap();
    let err = fx
        .store
        .queue_admit(&owner(), &bare.label, AdmitItem::default())
        .unwrap_err();
    assert_eq!(err.code(), "invalid");
    assert!(err.to_string().contains("it has no scope"), "{err}");
    let err = fx
        .store
        .queue_admit(
            &owner(),
            &bare.label,
            AdmitItem {
                scope: Some("M1".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert_eq!(err.code(), "invalid");
    assert_eq!(
        err.to_string(),
        format!(
            "{} cannot be admitted: it has no done condition. Pass --done-when.",
            bare.label
        )
    );
    assert_eq!(fx.item(&bare.label).state, QueueState::Proposed);
    assert!(
        fx.item(&bare.label).scope.is_none(),
        "a refused admit keeps nothing"
    );

    // By the owner.
    fx.advance(1000);
    let item = fx
        .store
        .queue_admit(
            &owner(),
            &bare.label,
            AdmitItem {
                scope: Some("M1".into()),
                done_when: Some("the tests pass".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let at = fmt_ts(T0 + 1000);
    assert_eq!(item.state, QueueState::Ready);
    let admitted = item.admitted.clone().expect("the approval");
    assert_eq!(
        (
            admitted.authority.as_str(),
            admitted.by.as_str(),
            &admitted.at
        ),
        ("owner", "owner", &at)
    );
    assert!(!admitted.self_admitted && admitted.edited_after.is_none());
    assert_eq!(
        item.approval_line(),
        Some(format!("admitted by owner at {at}"))
    );
    let v = serde_json::to_value(&item).unwrap();
    assert_eq!(
        v["admitted"],
        json!({ "authority": "owner", "by": "owner", "at": at, "self_admitted": false, "edited_after": null })
    );
    let (authority, by, when): (String, Option<String>, i64) = fx
        .db()
        .query_row(
            "SELECT admit_authority, admitted_by, admitted_at FROM queue_items WHERE seq = ?1",
            [item.seq],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((authority.as_str(), by, when), ("owner", None, T0 + 1000));
    let event = fx.events("queue.admitted").pop().unwrap();
    assert_eq!(event.actor, "owner");
    assert_eq!(event.ts, at);
    assert_eq!(event.payload["authority"], "owner");
    assert_eq!(event.payload["via"], "terminal");
    assert_eq!(event.payload["admitted_by"], Value::Null);
    assert_eq!(event.payload["item_id"], item.item_id.as_str());
    let history = item.history.clone().expect("history");
    let row = history.last().unwrap();
    assert_eq!(
        (row.kind.as_str(), row.actor.as_str(), &row.at),
        ("queue.admitted", "owner", &at)
    );
    assert_eq!(row.detail.as_ref().unwrap()["authority"], "owner");

    // By a delegate.
    let arch = fx.delegate();
    let second = fx.propose("ann", "second");
    fx.advance(1000);
    let second = fx
        .store
        .queue_admit(&arch, &second.label, AdmitItem::default())
        .unwrap();
    let at2 = fmt_ts(T0 + 2000);
    let admitted = second.admitted.clone().unwrap();
    assert_eq!(
        (
            admitted.authority.as_str(),
            admitted.by.as_str(),
            &admitted.at
        ),
        ("delegate", "arch", &at2)
    );
    assert_eq!(
        second.approval_line(),
        Some(format!("admitted by arch (delegate) at {at2}"))
    );
    let event = fx.events("queue.admitted").pop().unwrap();
    assert_eq!(event.actor, "arch");
    assert_eq!(event.payload["authority"], "delegate");
    assert_eq!(event.payload["via"], "key");
    assert_eq!(event.payload["admitted_by"], fx.agent_id("arch").as_str());

    // An edit of the scope or done condition after admission adds its line.
    fx.advance(1000);
    let edited = fx
        .store
        .queue_edit(
            &arch,
            &second.label,
            EditItem {
                done_when: Some("the tests pass on CI".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let at3 = fmt_ts(T0 + 3000);
    let after = edited
        .admitted
        .clone()
        .unwrap()
        .edited_after
        .expect("edited after");
    assert_eq!((after.by.as_str(), &after.at), ("arch", &at3));
    assert_eq!(
        edited.approval_line(),
        Some(format!(
            "admitted by arch (delegate) at {at2}\nedited by arch at {at3} after admission"
        ))
    );
    // An edit of something else does not.
    let item = fx
        .store
        .queue_edit(
            &owner(),
            &item.label,
            EditItem {
                priority: Some(QueuePriority::High),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(item.admitted.unwrap().edited_after.is_none());
}

// T8
#[test]
fn next_refuses_an_agent_at_its_limit_and_says_why() {
    let fx = Fx::new();
    let first = fx.ready("first");
    let second = fx.ready("second");
    assert_eq!(fx.claim("ann").label, first.label);
    let before = fx.snapshot();
    let claims = fx.events("queue.claimed").len();
    let out = fx.store.queue_next("ann").unwrap();
    assert!(out.claimed.is_none());
    let refused = out.refused.expect("a refusal");
    assert_eq!(refused.reason, "at_limit");
    assert_eq!(refused.limit, 1);
    assert_eq!(refused.held, vec![first.label.clone()]);
    for needle in [
        "ann holds 1 of its limit of 1",
        first.label.as_str(),
        "another queue done",
        "another queue release",
        "ANOTHER_AGENT",
    ] {
        assert!(
            refused.message.contains(needle),
            "{needle:?} missing from {:?}",
            refused.message
        );
    }
    assert_eq!(
        serde_json::to_value(fx.store.queue_next("ann").unwrap()).unwrap(),
        json!({
            "claimed": null,
            "refused": { "reason": "at_limit", "limit": 1, "held": [first.label], "message": refused.message }
        })
    );
    assert_eq!(fx.snapshot(), before, "a refused next claims nothing");
    assert_eq!(fx.events("queue.claimed").len(), claims);
    assert_eq!(fx.item(&second.label).state, QueueState::Ready);

    // The owner raises the limit: the next item is handed over.
    assert_eq!(
        code(fx.store.queue_set_limit(&owner(), "ann", 21)),
        "invalid"
    );
    assert_eq!(
        code(fx.store.queue_set_limit(&owner(), "ann", -1)),
        "invalid"
    );
    assert_eq!(
        code(fx.store.queue_set_limit(&agent("ann"), "ann", 2)),
        "forbidden"
    );
    fx.store.queue_set_limit(&owner(), "ann", 2).unwrap();
    assert_eq!(fx.claim("ann").label, second.label);
    let event = fx.events("queue.claimed").pop().unwrap();
    assert_eq!(
        (
            event.payload["held"].as_i64(),
            event.payload["limit"].as_i64()
        ),
        (Some(1), Some(2))
    );
    let set = fx.events("queue.limit_set").pop().unwrap();
    assert_eq!(set.payload["agent_id"], fx.agent_id("ann").as_str());
    assert_eq!(
        (set.payload["from"].as_i64(), set.payload["to"].as_i64()),
        (Some(1), Some(2))
    );

    // Lowering a limit below what the agent holds releases nothing.
    fx.store.queue_set_limit(&owner(), "ann", 1).unwrap();
    for label in [&first.label, &second.label] {
        assert_eq!(fx.item(label).state, QueueState::Claimed);
    }
    let refused = fx.store.queue_next("ann").unwrap().refused.unwrap();
    assert_eq!(
        (refused.reason.as_str(), refused.limit, refused.held.len()),
        ("at_limit", 1, 2)
    );

    // Limit 0 is no seat.
    fx.ready("third");
    fx.store.queue_set_limit(&owner(), "bob", 0).unwrap();
    let refused = fx.store.queue_next("bob").unwrap().refused.unwrap();
    assert_eq!((refused.reason.as_str(), refused.limit), ("no_seat", 0));
    assert!(refused.held.is_empty());
    assert!(
        refused
            .message
            .contains("another queue limit set bob 1 --owner"),
        "{}",
        refused.message
    );

    // The default applies to agents with no row of their own, and starts at 1.
    let limits = fx.store.queue_limits().unwrap();
    assert_eq!(limits.default_limit, 1);
    assert_eq!(
        code(fx.store.queue_set_default_limit(&agent("cy"), 3)),
        "forbidden"
    );
    fx.store.queue_set_default_limit(&owner(), 0).unwrap();
    let refused = fx.store.queue_next("cy").unwrap().refused.unwrap();
    assert_eq!(refused.reason, "no_seat");
    let set = fx.events("queue.limit_set").pop().unwrap();
    assert_eq!(set.payload["default"], true);
}

/// Insert an event as this node or another wrote it, bypassing the queue code.
fn seed_event(db: &Connection, node_id: &str, ts: i64, kind: &str, payload: Value) -> String {
    let event_id = ulid_like(db);
    db.execute(
        "INSERT INTO events (event_id, node_id, ts, actor, kind, payload) VALUES (?1, ?2, ?3, 'seed', ?4, ?5)",
        rusqlite::params![event_id, node_id, ts, kind, payload.to_string()],
    )
    .unwrap();
    event_id
}

fn ulid_like(db: &Connection) -> String {
    db.query_row("SELECT upper(hex(randomblob(13)))", [], |r| r.get(0))
        .unwrap()
}

fn foreign_node(db: &Connection) -> String {
    db.execute(
        "INSERT OR IGNORE INTO nodes (node_id, display_name, role, is_local, created_at)
         VALUES ('NODEFOREIGN', 'elsewhere', 'home', 0, 1)",
        [],
    )
    .unwrap();
    "NODEFOREIGN".to_string()
}

fn finding_kinds(audit: &QueueAudit) -> Vec<&str> {
    audit.findings.iter().map(|f| f.kind.as_str()).collect()
}

// T9
#[test]
fn audit_finds_duplicates_losses_and_unadmitted_claims() {
    // A run with every transition is clean.
    let clean = || {
        let fx = Fx::new();
        let a = fx.ready("a");
        let b = fx.ready("b");
        fx.claim("ann");
        fx.store.queue_note("bob", &a.label, "looks fine").unwrap();
        fx.store
            .queue_release(&agent("ann"), &a.label, ReleaseItem::default())
            .unwrap();
        fx.claim("bob");
        fx.store
            .queue_done("bob", &a.label, done("abc1234"))
            .unwrap();
        fx.advance(HOUR);
        fx.claim("cy");
        (fx, a, b)
    };
    let (fx, a, b) = clean();
    let audit = fx.store.queue_audit(None).unwrap();
    assert!(audit.ok, "{:?}", audit.findings);
    assert_eq!(
        (audit.items, audit.claims, audit.completions, audit.claimers),
        (2, 3, 1, 3)
    );
    assert_eq!(audit.releases.get("agent"), Some(&1));
    // `--since` limits the findings, not the events read: an item claimed before the cut
    // and still held gives no false finding.
    let audit = fx.store.queue_audit(Some(T0 + HOUR / 2)).unwrap();
    assert!(audit.ok, "{:?}", audit.findings);
    let node = fx.store.node_id().to_string();
    let cy = fx.agent_id("cy");

    // Another node's events are not this node's record.
    let db = fx.db();
    let foreign = foreign_node(&db);
    seed_event(
        &db,
        &foreign,
        T0,
        "queue.claimed",
        json!({ "item_id": a.item_id, "seq": a.seq, "claimer_id": cy, "held": 9, "limit": 1 }),
    );
    assert!(fx.store.queue_audit(None).unwrap().ok);

    // A second claim with no release between.
    seed_event(
        &db,
        &node,
        T0 + HOUR,
        "queue.claimed",
        json!({ "item_id": b.item_id, "seq": b.seq, "claimer_id": cy, "held": 0, "limit": 1 }),
    );
    let audit = fx.store.queue_audit(None).unwrap();
    assert!(!audit.ok);
    assert!(
        finding_kinds(&audit).contains(&"duplicate"),
        "{:?}",
        audit.findings
    );
    // It happened after the cut, so it is reported with `--since` too; a later cut hides it.
    assert!(!fx.store.queue_audit(Some(T0 + HOUR)).unwrap().ok);
    assert!(fx.store.queue_audit(Some(T0 + 2 * HOUR)).unwrap().ok);

    // A second claimer on the row.
    let (fx, _, b) = clean();
    fx.db()
        .execute(
            "UPDATE queue_items SET claimer_id = ?2 WHERE seq = ?1",
            rusqlite::params![b.seq, fx.agent_id("ann")],
        )
        .unwrap();
    let audit = fx.store.queue_audit(None).unwrap();
    assert_eq!(finding_kinds(&audit), ["loss"], "{:?}", audit.findings);

    // A deleted row.
    let (fx, _, b) = clean();
    let db = fx.db();
    db.execute("DELETE FROM queue_history WHERE item_id = ?1", [&b.item_id])
        .unwrap();
    db.execute("DELETE FROM queue_items WHERE seq = ?1", [b.seq])
        .unwrap();
    let audit = fx.store.queue_audit(None).unwrap();
    assert_eq!(finding_kinds(&audit), ["loss"], "{:?}", audit.findings);
    assert_eq!(audit.findings[0].item, b.label);

    // A changed state.
    let (fx, a, _) = clean();
    fx.db()
        .execute(
            "UPDATE queue_items SET state = 'ready', outcome = NULL WHERE seq = ?1",
            [a.seq],
        )
        .unwrap();
    let audit = fx.store.queue_audit(None).unwrap();
    assert_eq!(finding_kinds(&audit), ["loss"], "{:?}", audit.findings);

    // A removed history row.
    let (fx, a, _) = clean();
    fx.db()
        .execute(
            "DELETE FROM queue_history WHERE item_id = ?1 AND kind = 'queue.noted'",
            [&a.item_id],
        )
        .unwrap();
    let audit = fx.store.queue_audit(None).unwrap();
    assert_eq!(finding_kinds(&audit), ["loss"], "{:?}", audit.findings);

    // A claim at or over the limit, and a claim of an item nobody admitted.
    let fx = Fx::new();
    let proposed = fx.propose("ann", "never admitted");
    let db = fx.db();
    seed_event(
        &db,
        &node_of(&fx),
        T0,
        "queue.claimed",
        json!({ "item_id": proposed.item_id, "seq": proposed.seq, "claimer_id": fx.agent_id("bob"), "held": 1, "limit": 1 }),
    );
    let audit = fx.store.queue_audit(None).unwrap();
    let kinds = finding_kinds(&audit);
    assert!(kinds.contains(&"admission"), "{:?}", audit.findings);
    assert!(kinds.contains(&"limit"), "{:?}", audit.findings);
}

fn node_of(fx: &Fx) -> String {
    fx.store.node_id().to_string()
}

/// Make every later mail fail, with `marker` in the error.
fn break_mail(db: &Connection, marker: &str) {
    db.execute_batch(&format!(
        "CREATE TRIGGER break_mail BEFORE INSERT ON messages
         BEGIN SELECT RAISE(ABORT, '{marker}'); END;"
    ))
    .unwrap();
}

fn labels(items: &[QueueItem]) -> Vec<&str> {
    items.iter().map(|i| i.label.as_str()).collect()
}

// T10
#[test]
fn the_owner_list_shows_each_section() {
    let fx = Fx::new();
    let arch = fx.delegate();
    // 1: a fast-lane proposal. 2: an ordinary one.
    let ordinary = fx.propose("ann", "ordinary");
    let urgent = fx
        .store
        .queue_propose(
            &agent("bob"),
            ProposeItem {
                title: "a leak".into(),
                kind: QueueKind::Security,
                ..Default::default()
            },
        )
        .unwrap();
    let later = fx.propose("bob", "admitted soon");
    // 3: a claim that goes quiet.
    let quiet = fx.ready("goes quiet");
    assert_eq!(fx.claim("ann").label, quiet.label);
    // 6: a self-admitted item.
    let own = fx.propose("arch", "the delegate's own");
    fx.store
        .queue_admit(&arch, &own.label, AdmitItem::default())
        .unwrap();
    // 5: a notice that could not be sent.
    let taken = fx.ready("taken back");
    fx.store.queue_set_limit(&owner(), "bob", 2).unwrap();
    assert_eq!(fx.claim("bob").label, own.label);
    assert_eq!(fx.claim("bob").label, taken.label);
    break_mail(&fx.db(), "mail is broken");
    fx.store
        .queue_release(
            &owner(),
            &taken.label,
            ReleaseItem {
                reason: Some("wrong agent".into()),
                ..Default::default()
            },
        )
        .unwrap();
    fx.db().execute_batch("DROP TRIGGER break_mail").unwrap();
    fx.store
        .queue_done("bob", &own.label, done("abc1234"))
        .unwrap();
    // 4: reserved for an agent with no seat, and reserved for an agent nobody has seen.
    let no_seat = fx.ready("for an agent with no seat");
    let unseen = fx.ready("for an agent not seen");
    fx.store.queue_set_limit(&owner(), "arch", 3).unwrap();
    fx.store
        .queue_edit(
            &owner(),
            &no_seat.label,
            EditItem {
                for_agent: Some("arch".into()),
                ..Default::default()
            },
        )
        .unwrap();
    fx.store.queue_set_limit(&owner(), "arch", 0).unwrap();
    fx.store
        .queue_edit(
            &owner(),
            &unseen.label,
            EditItem {
                for_agent: Some("cy".into()),
                ..Default::default()
            },
        )
        .unwrap();

    // Two hours on: bob is still around, ann and cy are not.
    fx.advance(2 * HOUR);
    fx.store
        .queue_note("bob", &ordinary.label, "still here")
        .unwrap();
    // Another node's failed notice is not this node's business.
    let db = fx.db();
    let foreign = foreign_node(&db);
    seed_event(
        &db,
        &foreign,
        T0 + 2 * HOUR,
        "queue.notify_failed",
        json!({ "item_id": quiet.item_id, "seq": quiet.seq, "to": fx.agent_id("ann"), "reason": "owner", "error": "x" }),
    );

    let before = fx.snapshot();
    let list = fx.store.queue_needs_owner().unwrap();
    assert_eq!(fx.snapshot(), before, "the read writes nothing");
    assert_eq!(labels(&list.decide_now), [urgent.label.as_str()]);
    assert_eq!(
        labels(&list.awaiting),
        [ordinary.label.as_str(), later.label.as_str()],
        "oldest first, without the fast-lane item"
    );
    assert_eq!(list.quiet_claims.len(), 1, "{:?}", list.quiet_claims);
    let q = &list.quiet_claims[0];
    assert_eq!(q.item.label, quiet.label);
    assert_eq!(q.item.claim.as_ref().unwrap().agent, "ann");
    assert_eq!(q.quiet_for_seconds, 7200);
    assert_eq!(q.grace_ends_at, fmt_ts(T0 + DAY));
    assert!(q.unbound && q.session_last_seen_at.is_none());
    assert_eq!(
        labels(&list.nobody_can_take),
        [no_seat.label.as_str(), unseen.label.as_str()]
    );
    assert_eq!(list.failed_notices.len(), 1, "{:?}", list.failed_notices);
    assert_eq!(
        (
            list.failed_notices[0].item.as_str(),
            list.failed_notices[0].to.as_str()
        ),
        (taken.label.as_str(), "bob")
    );
    assert_eq!(labels(&list.self_admitted.recent), [own.label.as_str()]);
    assert_eq!(list.self_admitted.total, 1);
    let admitted = list.self_admitted.recent[0].admitted.as_ref().unwrap();
    assert_eq!(
        (admitted.by.as_str(), admitted.self_admitted),
        ("arch", true)
    );

    // The JSON is one object with a list per section, in this order.
    let v = serde_json::to_value(&list).unwrap();
    assert_eq!(
        v.as_object().unwrap().keys().collect::<Vec<_>>(),
        [
            "decide_now",
            "awaiting",
            "quiet_claims",
            "nobody_can_take",
            "failed_notices",
            "self_admitted"
        ]
    );
    assert_eq!(
        v["quiet_claims"][0]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        [
            "item",
            "quiet_for_seconds",
            "grace_ends_at",
            "unbound",
            "session_last_seen_at",
            "reason",
            "session_quiet_for_seconds"
        ]
    );
    assert_eq!(
        v["failed_notices"][0]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["item", "to", "at"]
    );
    assert_eq!(
        v["self_admitted"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["recent", "total"]
    );

    // An admitted item leaves the list of pending decisions; so does the fast-lane one.
    fx.store
        .queue_admit(&owner(), &later.label, AdmitItem::default())
        .unwrap();
    fx.store
        .queue_reject(&owner(), &urgent.label, "not a leak")
        .unwrap();
    let list = fx.store.queue_needs_owner().unwrap();
    assert_eq!(labels(&list.awaiting), [ordinary.label.as_str()]);
    assert!(list.decide_now.is_empty());
    // A failed notice is listed for a day.
    fx.advance(DAY + HOUR);
    assert!(fx
        .store
        .queue_needs_owner()
        .unwrap()
        .failed_notices
        .is_empty());
}

// T11
#[test]
fn next_order_reservation_and_skip() {
    let fx = Fx::new();
    fx.store.queue_set_default_limit(&owner(), 10).unwrap();
    // High before normal, then admission time, then insertion order within one millisecond.
    let p1 = fx.propose("ann", "proposed first, admitted last");
    let p2 = fx.propose("ann", "normal a");
    let p3 = fx.propose("ann", "normal b");
    let p4 = fx.propose("ann", "high");
    for p in [&p2, &p3] {
        fx.store
            .queue_admit(&owner(), &p.label, AdmitItem::default())
            .unwrap();
    }
    fx.advance(10);
    fx.store
        .queue_admit(
            &owner(),
            &p4.label,
            AdmitItem {
                priority: Some(QueuePriority::High),
                ..Default::default()
            },
        )
        .unwrap();
    fx.advance(10);
    fx.store
        .queue_admit(&owner(), &p1.label, AdmitItem::default())
        .unwrap();
    let order: Vec<String> = (0..4).map(|_| fx.claim("ann").label).collect();
    assert_eq!(
        order,
        [
            p4.label.as_str(),
            p2.label.as_str(),
            p3.label.as_str(),
            p1.label.as_str()
        ]
    );
    // A released item keeps its admission time, so it sorts ahead of newer ones.
    fx.advance(10);
    let newer = fx.ready("newer");
    fx.store
        .queue_release(&agent("ann"), &p2.label, ReleaseItem::default())
        .unwrap();
    assert_eq!(fx.claim("bob").label, p2.label);
    for label in [&p4.label, &p3.label, &p1.label] {
        fx.store.queue_done("ann", label, done("abc1234")).unwrap();
    }
    fx.store
        .queue_done("bob", &p2.label, done("abc1234"))
        .unwrap();

    // Skip: the agent that gave an item up is not offered it again.
    assert_eq!(fx.claim("ann").label, newer.label);
    let released = fx
        .store
        .queue_release(&agent("ann"), &newer.label, ReleaseItem::default())
        .unwrap();
    assert_eq!(released.skip_agent.as_deref(), Some("ann"));
    let out = fx.store.queue_next("ann").unwrap();
    assert!(
        out.claimed.is_none() && out.refused.is_none(),
        "ann is skipped"
    );
    // An edit that sets or clears the reservation clears the skip.
    let cleared = fx
        .store
        .queue_edit(
            &owner(),
            &newer.label,
            EditItem {
                for_anyone: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(cleared.skip_agent, None);
    assert_eq!(fx.claim("ann").label, newer.label);
    fx.store
        .queue_done("ann", &newer.label, done("abc1234"))
        .unwrap();

    // Reservation: only the reserved agent is offered the item, and it is a first offer.
    let kept = fx.propose("ann", "kept for bob");
    let kept = fx
        .store
        .queue_admit(
            &owner(),
            &kept.label,
            AdmitItem {
                for_agent: Some("bob".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(kept.for_agent.as_deref(), Some("bob"));
    assert!(fx.store.queue_next("ann").unwrap().claimed.is_none());
    assert_eq!(fx.claim("bob").label, kept.label);
    let released = fx
        .store
        .queue_release(&agent("bob"), &kept.label, ReleaseItem::default())
        .unwrap();
    assert_eq!(
        released.for_agent, None,
        "the reservation ends with the claim"
    );
    assert_eq!(
        fx.events("queue.released").pop().unwrap().payload["for_agent_cleared"],
        fx.agent_id("bob").as_str()
    );
    assert_eq!(fx.claim("ann").label, kept.label);

    // Handoff then release: the named agent takes it with `next`; when it gives the item
    // up, the first agent can take it again.
    let handed = fx
        .store
        .queue_release(
            &agent("ann"),
            &kept.label,
            ReleaseItem {
                to: Some("cy".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        (handed.for_agent.as_deref(), handed.skip_agent.as_deref()),
        (Some("cy"), Some("ann"))
    );
    assert!(fx.store.queue_next("bob").unwrap().claimed.is_none());
    assert_eq!(fx.claim("cy").label, kept.label);
    fx.store
        .queue_release(&agent("cy"), &kept.label, ReleaseItem::default())
        .unwrap();
    assert_eq!(fx.claim("ann").label, kept.label);

    // `--for` and `--to` name another local agent with a seat.
    fx.store.queue_set_limit(&owner(), "cy", 0).unwrap();
    let to = |name: &str| ReleaseItem {
        to: Some(name.into()),
        ..Default::default()
    };
    assert_eq!(
        code(fx.store.queue_release(&agent("ann"), &kept.label, to("cy"))),
        "invalid"
    );
    assert_eq!(
        code(
            fx.store
                .queue_release(&agent("ann"), &kept.label, to("ann"))
        ),
        "invalid"
    );
    assert_eq!(
        code(
            fx.store
                .queue_release(&agent("ann"), &kept.label, to("nobody"))
        ),
        "not_found"
    );
    let other = fx.ready("other");
    let reserve = |name: &str| EditItem {
        for_agent: Some(name.into()),
        ..Default::default()
    };
    assert_eq!(
        code(fx.store.queue_edit(&owner(), &other.label, reserve("cy"))),
        "invalid"
    );
    assert_eq!(
        code(
            fx.store
                .queue_edit(&owner(), &other.label, reserve("nobody"))
        ),
        "not_found"
    );
    let arch = fx.delegate();
    assert_eq!(
        code(fx.store.queue_edit(&arch, &other.label, reserve("arch"))),
        "invalid",
        "a delegate cannot reserve an item for itself"
    );
    assert_eq!(fx.item(&kept.label).state, QueueState::Claimed);
}

// T12
#[test]
fn an_empty_or_refused_next_claims_nothing() {
    let fx = Fx::new();
    fx.propose("ann", "only proposed");
    let before = fx.snapshot();
    let out = fx.store.queue_next("ann").unwrap();
    assert!(out.claimed.is_none() && out.refused.is_none());
    assert_eq!(
        serde_json::to_value(&out).unwrap(),
        json!({ "claimed": null, "refused": null })
    );
    assert_eq!(fx.snapshot(), before);
    assert!(fx.events("queue.claimed").is_empty());

    fx.ready("one");
    fx.ready("two");
    fx.claim("ann");
    let before = fx.snapshot();
    assert!(fx.store.queue_next("ann").unwrap().refused.is_some());
    assert_eq!(fx.snapshot(), before);
    assert_eq!(fx.events("queue.claimed").len(), 1);
    assert_eq!(code(fx.store.queue_next("nobody")), "not_found");
}

/// A fixture with one item in `state`: `proposed`, `ready`, `claimed` (by ann),
/// `completed` or `rejected`.
fn in_state(state: &str) -> (Fx, String, QueueCaller) {
    let fx = Fx::new();
    let arch = fx.delegate();
    let label = fx.propose("ann", "the item").label;
    if state != "proposed" && state != "rejected" {
        fx.store
            .queue_admit(&owner(), &label, AdmitItem::default())
            .unwrap();
    }
    if state == "claimed" || state == "completed" {
        fx.claim("ann");
    }
    if state == "completed" {
        fx.store.queue_done("ann", &label, done("abc1234")).unwrap();
    }
    if state == "rejected" {
        fx.store.queue_reject(&owner(), &label, "no").unwrap();
    }
    (fx, label, arch)
}

// T13
#[test]
fn transitions_follow_the_table() {
    let ops = [
        "admit",
        "reject",
        "edit",
        "note",
        "release",
        "handoff",
        "admin_release",
        "done",
    ];
    let allowed = |state: &str, op: &str| match op {
        "admit" => state == "proposed",
        "reject" | "edit" => state == "proposed" || state == "ready",
        "note" => state != "completed" && state != "rejected",
        _ => state == "claimed",
    };
    for state in ["proposed", "ready", "claimed", "completed", "rejected"] {
        for op in ops {
            for by_delegate in [false, true] {
                let (fx, label, arch) = in_state(state);
                let admitter = if by_delegate { arch } else { owner() };
                let before = fx.snapshot();
                let result: Result<QueueItem> = match op {
                    "admit" => fx
                        .store
                        .queue_admit(&admitter, &label, AdmitItem::default()),
                    "reject" => fx.store.queue_reject(&admitter, &label, "not wanted"),
                    "edit" => fx.store.queue_edit(
                        &admitter,
                        &label,
                        EditItem {
                            scope: Some("M2".into()),
                            ..Default::default()
                        },
                    ),
                    "note" => fx.store.queue_note("bob", &label, "a note"),
                    "release" => {
                        fx.store
                            .queue_release(&agent("ann"), &label, ReleaseItem::default())
                    }
                    "handoff" => fx.store.queue_release(
                        &agent("ann"),
                        &label,
                        ReleaseItem {
                            to: Some("bob".into()),
                            ..Default::default()
                        },
                    ),
                    "admin_release" => fx.store.queue_release(
                        &admitter,
                        &label,
                        ReleaseItem {
                            reason: Some("needed elsewhere".into()),
                            ..Default::default()
                        },
                    ),
                    _ => fx.store.queue_done("ann", &label, done("abc1234")),
                };
                if allowed(state, op) {
                    result.unwrap_or_else(|e| panic!("{op} on a {state} item: {e}"));
                    assert_ne!(fx.snapshot(), before, "{op} on a {state} item");
                } else {
                    let err = result.expect_err(&format!("{op} on a {state} item must fail"));
                    assert!(
                        ["conflict", "forbidden"].contains(&err.code()),
                        "{op} on a {state} item: {err}"
                    );
                    assert_eq!(
                        fx.snapshot(),
                        before,
                        "{op} on a {state} item changed something"
                    );
                }
            }
        }
    }

    // A wrong-state refusal names the state and the claimer.
    let (fx, label, _) = in_state("claimed");
    let err = fx.store.queue_reject(&owner(), &label, "no").unwrap_err();
    assert_eq!(err.code(), "conflict");
    assert!(err.to_string().contains("claimed by ann"), "{err}");
    // Only the claimer finishes or gives back; an agent that is no admitter decides nothing.
    assert_eq!(
        code(fx.store.queue_done("bob", &label, done("abc1234"))),
        "forbidden"
    );
    assert_eq!(
        code(
            fx.store
                .queue_release(&agent("bob"), &label, ReleaseItem::default())
        ),
        "forbidden"
    );
    let (fx, label, _) = in_state("proposed");
    assert_eq!(
        code(
            fx.store
                .queue_admit(&agent("bob"), &label, AdmitItem::default())
        ),
        "forbidden"
    );
    assert_eq!(
        code(fx.store.queue_reject(&agent("bob"), &label, "no")),
        "forbidden"
    );
    assert_eq!(code(fx.store.queue_item("Q99")), "not_found");
    assert_eq!(fx.item(&label.to_lowercase()).label, label, "q1 names Q1");
    let by_id = fx.item(&fx.item(&label).item_id);
    assert_eq!(by_id.label, label);

    // Unsafe text is refused in every field; line breaks and tabs pass in the multi-line ones.
    let fx = Fx::new();
    let arch = fx.delegate();
    let propose = |title: &str, body: &str, scope: &str, done_when: &str| {
        fx.store.queue_propose(
            &agent("ann"),
            ProposeItem {
                title: title.into(),
                body: body.into(),
                scope: Some(scope.into()),
                done_when: Some(done_when.into()),
                ..Default::default()
            },
        )
    };
    let lines = "first line\n\tsecond line";
    for bad in ["a\u{1b}[2Jb", "a\u{202e}b", "a\u{7f}b", "a\rb"] {
        assert_eq!(
            code(propose(bad, "b", "M1", "d")),
            "invalid",
            "title {bad:?}"
        );
        assert_eq!(
            code(propose("t", bad, "M1", "d")),
            "invalid",
            "body {bad:?}"
        );
        assert_eq!(
            code(propose("t", "b", bad, "d")),
            "invalid",
            "scope {bad:?}"
        );
        assert_eq!(
            code(propose("t", "b", "M1", bad)),
            "invalid",
            "done condition {bad:?}"
        );
    }
    assert_eq!(code(propose("two\nlines", "b", "M1", "d")), "invalid");
    assert_eq!(code(propose("a\ttab", "b", "M1", "d")), "invalid");
    assert_eq!(
        code(propose("t", "b", "M 1", "d")),
        "invalid",
        "a scope id has no spaces"
    );
    assert_eq!(code(propose("t", "b", &"M".repeat(33), "d")), "invalid");
    assert_eq!(code(propose("", "b", "M1", "d")), "invalid");
    assert_eq!(code(propose(&"t".repeat(201), "b", "M1", "d")), "invalid");
    assert_eq!(
        code(propose("t", &"b".repeat(10_001), "M1", "d")),
        "invalid"
    );
    assert_eq!(code(propose("t", "b", "M1", &"d".repeat(1001))), "invalid");
    let item = propose("t", lines, "M1", lines).unwrap();
    for bad in ["a\u{1b}[2Jb", "a\u{202e}b"] {
        assert_eq!(
            code(fx.store.queue_note("bob", &item.label, bad)),
            "invalid"
        );
        assert_eq!(
            code(fx.store.queue_reject(&owner(), &item.label, bad)),
            "invalid"
        );
        assert_eq!(
            code(fx.store.queue_admit(
                &owner(),
                &item.label,
                AdmitItem {
                    done_when: Some(bad.into()),
                    ..Default::default()
                }
            )),
            "invalid"
        );
        assert_eq!(
            code(fx.store.queue_edit(
                &owner(),
                &item.label,
                EditItem {
                    scope: Some(bad.into()),
                    ..Default::default()
                }
            )),
            "invalid"
        );
    }
    assert_eq!(code(fx.store.queue_note("bob", &item.label, "")), "invalid");
    assert_eq!(
        code(fx.store.queue_note("bob", &item.label, &"n".repeat(2001))),
        "invalid"
    );
    assert_eq!(
        code(
            fx.store
                .queue_reject(&owner(), &item.label, &"r".repeat(501))
        ),
        "invalid"
    );
    fx.store.queue_note("bob", &item.label, lines).unwrap();
    fx.store
        .queue_admit(&owner(), &item.label, AdmitItem::default())
        .unwrap();
    fx.claim("ann");
    for bad in ["a\u{1b}[2Jb", "a\u{202e}b"] {
        let note = ReleaseItem {
            note: Some(bad.into()),
            ..Default::default()
        };
        assert_eq!(
            code(fx.store.queue_release(&agent("ann"), &item.label, note)),
            "invalid"
        );
        let reason = ReleaseItem {
            reason: Some(bad.into()),
            ..Default::default()
        };
        assert_eq!(
            code(fx.store.queue_release(&owner(), &item.label, reason)),
            "invalid"
        );
        let summary = DoneItem {
            summary: bad.into(),
            ..done("abc1234")
        };
        assert_eq!(
            code(fx.store.queue_done("ann", &item.label, summary)),
            "invalid"
        );
        let pointer = DoneItem {
            summary: "ok".into(),
            pointer_kind: PointerKind::File,
            pointer: bad.into(),
        };
        assert_eq!(
            code(fx.store.queue_done("ann", &item.label, pointer)),
            "invalid"
        );
    }
    let multi = DoneItem {
        summary: lines.into(),
        ..done("abc1234")
    };
    fx.store.queue_done("ann", &item.label, multi).unwrap();

    // Kind. While an item is proposed, any admitter may raise it; then it is the owner's.
    let raise = |kind: QueueKind| EditItem {
        kind: Some(kind),
        ..Default::default()
    };
    let p = fx.propose("ann", "turns out to be a leak");
    let raised = fx
        .store
        .queue_edit(&arch, &p.label, raise(QueueKind::Security))
        .unwrap();
    assert_eq!(raised.kind, QueueKind::Security);
    // Only the owner lowers a kind, or does anything else to a fast-lane item.
    let err = fx
        .store
        .queue_edit(&arch, &p.label, raise(QueueKind::Work))
        .unwrap_err();
    assert_eq!(err.code(), "forbidden");
    let err = fx
        .store
        .queue_admit(&arch, &p.label, AdmitItem::default())
        .unwrap_err();
    assert_eq!(err.code(), "forbidden");
    assert_eq!(
        err.to_string(),
        "security and data-loss items are decided by the owner personally"
    );
    assert_eq!(
        code(fx.store.queue_reject(&arch, &p.label, "no")),
        "forbidden"
    );
    let lowered = fx
        .store
        .queue_edit(&owner(), &p.label, raise(QueueKind::Work))
        .unwrap();
    assert_eq!(lowered.kind, QueueKind::Work);
    // On a ready item a delegate raises nothing.
    let by_delegate = fx.propose("ann", "admitted by the delegate");
    fx.store
        .queue_admit(&arch, &by_delegate.label, AdmitItem::default())
        .unwrap();
    assert_eq!(
        code(
            fx.store
                .queue_edit(&arch, &by_delegate.label, raise(QueueKind::DataLoss))
        ),
        "forbidden"
    );
    // The owner raises a ready item only if the owner admitted it.
    let err = fx
        .store
        .queue_edit(&owner(), &by_delegate.label, raise(QueueKind::Security))
        .unwrap_err();
    assert_eq!(err.code(), "forbidden");
    assert_eq!(
        err.to_string(),
        format!(
            "{} was admitted by a delegate. Reject it and propose it again as security.",
            by_delegate.label
        )
    );
    assert_eq!(fx.item(&by_delegate.label).kind, QueueKind::Work);
    let by_owner = fx.ready("admitted by the owner");
    let raised = fx
        .store
        .queue_edit(&owner(), &by_owner.label, raise(QueueKind::DataLoss))
        .unwrap();
    assert_eq!(raised.kind, QueueKind::DataLoss);
    let updated = fx.events("queue.updated").pop().unwrap();
    assert_eq!(
        updated.payload["kind"],
        json!({ "from": "work", "to": "data_loss" })
    );
    // A delegate does not release another agent's claim on a fast-lane item.
    fx.store.queue_set_limit(&owner(), "bob", 5).unwrap();
    while let Some(item) = fx.store.queue_next("bob").unwrap().claimed {
        if item.label == by_owner.label {
            break;
        }
    }
    assert_eq!(fx.item(&by_owner.label).state, QueueState::Claimed);
    let reason = ReleaseItem {
        reason: Some("needed".into()),
        ..Default::default()
    };
    assert_eq!(
        code(fx.store.queue_release(&arch, &by_owner.label, reason)),
        "forbidden"
    );
}

// T14
#[test]
fn queue_events_never_leave_the_node() {
    let dir = tempfile::tempdir().unwrap();
    let node = |name: &str| {
        let (clock, _) = Clock::manual(T0);
        let store = Store::open_with_clock(&dir.path().join(name), clock).unwrap();
        store.set_node_name(name).unwrap();
        store
    };
    let hub = node("hub");
    let spoke = node("spoke");
    let code = hub
        .create_invite("https://hub.example", &[Perm::ReadFeed], 3600)
        .unwrap();
    let (invite, req) = spoke.prepare_join(&code).unwrap();
    let resp = hub.accept_pairing(&req).unwrap();
    spoke.complete_join(&invite, &resp).unwrap();

    // The same run on both nodes: every queue event kind, and one direct request.
    let mut written = BTreeSet::new();
    for store in [&hub, &spoke] {
        for name in ["ann", "bob", "arch"] {
            register(store, name);
        }
        let made = store.queue_delegate_add(&owner(), "arch").unwrap();
        let arch = QueueCaller::Agent {
            agent: "arch".into(),
            admit_key: Some(made.key),
        };
        store.queue_set_limit(&owner(), "ann", 3).unwrap();
        store.queue_set_grace(&owner(), 7200).unwrap();
        let propose = |title: &str| {
            store
                .queue_propose(
                    &agent("ann"),
                    ProposeItem {
                        title: title.into(),
                        scope: Some("M1".into()),
                        done_when: Some("done".into()),
                        ..Default::default()
                    },
                )
                .unwrap()
                .label
        };
        let a = propose("a");
        let b = propose("b");
        let c = propose("c");
        store.queue_admit(&arch, &a, AdmitItem::default()).unwrap();
        store
            .queue_admit(&owner(), &c, AdmitItem::default())
            .unwrap();
        store.queue_reject(&owner(), &b, "no").unwrap();
        store
            .queue_edit(
                &owner(),
                &a,
                EditItem {
                    priority: Some(QueuePriority::High),
                    ..Default::default()
                },
            )
            .unwrap();
        store.queue_next("ann").unwrap().claimed.unwrap();
        store.queue_note("ann", &a, "a note").unwrap();
        store
            .queue_release(
                &agent("ann"),
                &a,
                ReleaseItem {
                    to: Some("bob".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        store.queue_next("bob").unwrap().claimed.unwrap();
        // The notice for this admin release cannot be sent.
        let db = Connection::open(store.db_path().unwrap()).unwrap();
        break_mail(&db, "mail is broken");
        store
            .queue_release(
                &arch,
                &a,
                ReleaseItem {
                    reason: Some("back".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        db.execute_batch("DROP TRIGGER break_mail").unwrap();
        store.queue_next("ann").unwrap().claimed.unwrap();
        store.queue_next("ann").unwrap().claimed.unwrap();
        store.queue_done("ann", &c, done("abc1234")).unwrap();
        store.queue_delegate_remove(&owner(), "arch").unwrap();
        store
            .send_message(SendMessage {
                from: "ann".into(),
                to: vec!["bob".into()],
                subject: "a direct request".into(),
                body: "please".into(),
                request: Some(NewRequest::default()),
                ..Default::default()
            })
            .unwrap();
        for e in store.events(100_000).unwrap() {
            if e.kind.starts_with("queue.") {
                written.insert(e.kind);
            }
        }
    }
    let every: BTreeSet<String> = [
        "queue.proposed",
        "queue.admitted",
        "queue.rejected",
        "queue.updated",
        "queue.claimed",
        "queue.noted",
        "queue.released",
        "queue.handoff",
        "queue.completed",
        "queue.notify_failed",
        "queue.limit_set",
        "queue.delegate_added",
        "queue.delegate_removed",
        "queue.setting_changed",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(written, every, "the run writes every queue event kind");

    // The spoke's push and the hub's answer to its pull carry the request and no queue event.
    let (outbound, _) = spoke.outbound_events(0, 100_000).unwrap();
    let pulled = hub.pull_for(spoke.node_id(), 0, 100_000).unwrap().events;
    for (what, events) in [("pushed", &outbound), ("pulled", &pulled)] {
        assert!(
            events.iter().any(|e| e.kind == "request.created"),
            "a direct request is {what}"
        );
        let leaked: Vec<&str> = events
            .iter()
            .filter(|e| e.kind.starts_with("queue."))
            .map(|e| e.kind.as_str())
            .collect();
        assert!(leaked.is_empty(), "{what}: {leaked:?}");
    }
}

const QUEUE_TABLES: [&str; 6] = [
    "queue_items",
    "queue_history",
    "queue_sessions",
    "queue_limits",
    "queue_delegates",
    "queue_settings",
];

fn table_counts(db: &Connection) -> Vec<(String, i64)> {
    let names: Vec<String> = db
        .prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    names
        .into_iter()
        .map(|n| {
            let count = db
                .query_row(&format!("SELECT count(*) FROM \"{n}\""), [], |r| r.get(0))
                .unwrap();
            (n, count)
        })
        .collect()
}

// T15
#[test]
fn migration_adds_only_queue_tables() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let store = Store::open(&home).unwrap();
    for name in ["ann", "bob"] {
        register(&store, name);
    }
    let request = store
        .send_message(SendMessage {
            from: "ann".into(),
            to: vec!["bob".into()],
            subject: "an open request".into(),
            body: "from before the queue".into(),
            request: Some(NewRequest::default()),
            ..Default::default()
        })
        .unwrap()
        .request_id
        .unwrap();
    drop(store);

    // Take the home back to the version before the queue: the same rows, without its tables.
    let db = Connection::open(home.join(DB_FILE)).unwrap();
    let current: i64 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    for table in QUEUE_TABLES {
        db.execute_batch(&format!("DROP TABLE {table}")).unwrap();
    }
    // The queue is schema v11. What later schemas added goes too, and comes back with them.
    db.execute_batch(
        "DROP TABLE transport_sessions;
         ALTER TABLE project_repos DROP COLUMN checkout;",
    )
    .unwrap();
    db.pragma_update(None, "user_version", 10).unwrap();
    let before = table_counts(&db);
    assert!(before.iter().all(|(n, _)| !n.starts_with("queue_")));
    drop(db);

    let store = Store::open(&home).unwrap();
    let db = Connection::open(home.join(DB_FILE)).unwrap();
    let version: i64 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, current);
    let after = table_counts(&db);
    let kept: Vec<_> = after
        .iter()
        .filter(|(n, _)| !n.starts_with("queue_") && n != "transport_sessions")
        .cloned()
        .collect();
    assert_eq!(kept, before, "every existing table keeps every row");
    let added: Vec<(&str, i64)> = after
        .iter()
        .filter(|(n, _)| n.starts_with("queue_"))
        .map(|(n, c)| (n.as_str(), *c))
        .collect();
    let mut expected: Vec<(&str, i64)> = QUEUE_TABLES.iter().map(|t| (*t, 0)).collect();
    expected.sort();
    assert_eq!(added, expected, "the six queue tables, all empty");
    // Open requests stay requests, and the queue works on the upgraded home.
    assert_eq!(store.request(&request).unwrap().status, RequestStatus::Open);
    assert_eq!(code(store.queue_item(&request)), "not_found");
    let item = store
        .queue_propose(
            &agent("ann"),
            ProposeItem {
                title: "first".into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(item.label, "Q1");
    assert_eq!(store.queue_limits().unwrap().default_limit, 1);
    assert_eq!(store.queue_grace().unwrap().grace_seconds, 86_400);
    assert!(store.queue_delegates().unwrap().is_empty());
}

// T16
#[test]
fn handoff_is_an_event() {
    let fx = Fx::new();
    let item = fx.ready("NOTITLE in the notice");
    fx.claim("ann");
    fx.advance(1000);
    let handed = fx
        .store
        .queue_release(
            &agent("ann"),
            &item.label,
            ReleaseItem {
                to: Some("bob".into()),
                note: Some("NOTETEXT half done, see the branch".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(handed.state, QueueState::Ready);
    assert!(handed.claim.is_none());
    let unread = fx.unread("bob");
    assert_eq!(unread.len(), 1);
    let mail = &unread[0].message;
    assert_eq!(mail.subject, format!("Handoff: {}", item.label));
    assert_eq!(mail.from, "ann");
    assert_eq!(
        mail.body,
        format!("ann handed {} to you. Run another queue next.", item.label)
    );
    assert!(!mail.body.contains("NOTETEXT") && !mail.body.contains("NOTITLE"));
    assert!(!mail.ack_required);

    let event = fx.events("queue.handoff").pop().expect("the handoff event");
    assert_eq!(event.actor, "ann");
    assert_eq!(event.payload["item_id"], item.item_id.as_str());
    assert_eq!(event.payload["from"], fx.agent_id("ann").as_str());
    assert_eq!(event.payload["to"], fx.agent_id("bob").as_str());
    assert_eq!(event.payload["note"], "NOTETEXT half done, see the branch");
    assert_eq!(event.payload["notice_message_id"], mail.message_id.as_str());
    assert_eq!(event.payload["claimed_at"].as_i64(), Some(T0));
    assert!(
        fx.events("queue.released").is_empty(),
        "a handoff is its own event"
    );
    // The history keeps the claim, the note and the handoff.
    let kinds: Vec<String> = handed
        .history
        .unwrap()
        .into_iter()
        .map(|h| h.kind)
        .collect();
    assert_eq!(
        kinds,
        [
            "queue.proposed",
            "queue.admitted",
            "queue.claimed",
            "queue.handoff"
        ]
    );
    // The named agent still takes it with `next`, and sees the note.
    let taken = fx.claim("bob");
    let note = taken
        .history
        .unwrap()
        .into_iter()
        .find(|h| h.kind == "queue.handoff")
        .unwrap()
        .note;
    assert_eq!(note.as_deref(), Some("NOTETEXT half done, see the branch"));
}

// T26
#[test]
fn a_result_is_stored_on_the_item() {
    let fx = Fx::new();
    let item = fx.ready("ship it");
    fx.claim("ann");
    let with = |kind: PointerKind, pointer: &str| DoneItem {
        summary: "Merged the fix".into(),
        pointer_kind: kind,
        pointer: pointer.into(),
    };
    // One valid pointer.
    for (kind, bad) in [
        (PointerKind::Pr, "http://example.com/pull/1"),
        (PointerKind::Pr, "12a"),
        (PointerKind::Pr, ""),
        (PointerKind::Pr, "https://example.com/pull 1"),
        (PointerKind::Commit, "abc12"),
        (PointerKind::Commit, "xyz1234"),
        (PointerKind::Commit, &"a".repeat(65)),
        (PointerKind::File, ""),
        (PointerKind::File, &"p".repeat(501)),
        (PointerKind::File, "two\nlines"),
    ] {
        assert_eq!(
            code(fx.store.queue_done("ann", &item.label, with(kind, bad))),
            "invalid",
            "{kind:?} {bad:?}"
        );
    }
    let empty = DoneItem {
        summary: " ".into(),
        ..done("abc1234")
    };
    assert_eq!(
        code(fx.store.queue_done("ann", &item.label, empty)),
        "invalid"
    );
    let long = DoneItem {
        summary: "s".repeat(501),
        ..done("abc1234")
    };
    assert_eq!(
        code(fx.store.queue_done("ann", &item.label, long)),
        "invalid"
    );
    // The claimer's alone.
    assert_eq!(
        code(
            fx.store
                .queue_done("bob", &item.label, with(PointerKind::Pr, "#12"))
        ),
        "forbidden"
    );
    assert_eq!(fx.item(&item.label).state, QueueState::Claimed);

    fx.advance(5000);
    let finished = fx
        .store
        .queue_done("ann", &item.label, with(PointerKind::Pr, "#12"))
        .unwrap();
    assert_eq!(finished.state, QueueState::Done);
    assert!(finished.claim.is_none());
    assert_eq!(
        serde_json::to_value(&finished).unwrap()["result"],
        json!({
            "outcome": "completed", "summary": "Merged the fix", "pointer_kind": "pr",
            "pointer": "#12", "by": "ann", "at": fmt_ts(T0 + 5000)
        })
    );
    let row: (String, String, String, String) = fx
        .db()
        .query_row(
            "SELECT outcome, result_summary, result_kind, result_ref FROM queue_items WHERE seq = ?1",
            [item.seq],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        row,
        (
            "completed".into(),
            "Merged the fix".into(),
            "pr".into(),
            "#12".into()
        )
    );
    let event = fx.events("queue.completed").pop().unwrap();
    assert_eq!(event.actor, "ann");
    assert_eq!(event.payload["summary"], "Merged the fix");
    assert_eq!(event.payload["pointer_kind"], "pr");
    assert_eq!(event.payload["pointer"], "#12");
    assert_eq!(event.payload["claimer_id"], fx.agent_id("ann").as_str());
    assert_eq!(event.payload["claimed_at"].as_i64(), Some(T0));
    // It is listed with its result, and no mail is sent.
    let listed = fx
        .store
        .queue_list(QueueFilter {
            state: Some(QueueState::Done),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].result.as_ref().unwrap().summary.as_deref(),
        Some("Merged the fix")
    );
    for who in ["ann", "bob", "cy", "arch"] {
        assert!(fx.unread(who).is_empty(), "{who} got mail");
    }
    // Done is final.
    assert_eq!(
        code(fx.store.queue_done("ann", &item.label, done("abc1234"))),
        "conflict"
    );
    // The other pointers.
    fx.ready("two");
    fx.ready("three");
    let two = fx.claim("ann");
    fx.store
        .queue_done(
            "ann",
            &two.label,
            with(PointerKind::Pr, "https://github.com/tamler/another/pull/12"),
        )
        .unwrap();
    let three = fx.claim("ann");
    let finished = fx
        .store
        .queue_done(
            "ann",
            &three.label,
            with(PointerKind::File, "docs/QUEUE.md"),
        )
        .unwrap();
    let result = finished.result.unwrap();
    assert_eq!(
        (result.pointer_kind.as_deref(), result.pointer.as_deref()),
        (Some("file"), Some("docs/QUEUE.md"))
    );
}

// T28
#[test]
fn a_self_admitted_item_is_marked_and_stays_listed() {
    let fx = Fx::new();
    let arch = fx.delegate();
    let own = fx.propose("arch", "the delegate's own");
    let own = fx
        .store
        .queue_admit(&arch, &own.label, AdmitItem::default())
        .unwrap();
    let admitted = own.admitted.clone().unwrap();
    assert!(admitted.self_admitted);
    assert_eq!(
        own.approval_line(),
        Some(format!(
            "admitted by arch (delegate, self-admitted) at {}",
            admitted.at
        ))
    );
    assert_eq!(
        serde_json::to_value(&own).unwrap()["admitted"]["self_admitted"],
        true
    );
    assert_eq!(
        fx.events("queue.admitted").pop().unwrap().payload["self_admitted"],
        true
    );

    // Another agent's proposal admitted by the delegate, and the owner's own, are not.
    let other = fx.propose("ann", "ann's");
    let other = fx
        .store
        .queue_admit(&arch, &other.label, AdmitItem::default())
        .unwrap();
    assert!(!other.admitted.unwrap().self_admitted);
    assert_eq!(
        fx.events("queue.admitted").pop().unwrap().payload["self_admitted"],
        false
    );
    let by_owner = fx
        .store
        .queue_propose(
            &owner(),
            ProposeItem {
                title: "the owner's".into(),
                scope: Some("M1".into()),
                done_when: Some("done".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(by_owner.proposed_by, "owner");
    let by_owner = fx
        .store
        .queue_admit(&owner(), &by_owner.label, AdmitItem::default())
        .unwrap();
    assert!(!by_owner.admitted.unwrap().self_admitted);

    let self_admitted = || {
        fx.store
            .queue_list(QueueFilter {
                self_admitted: true,
                ..Default::default()
            })
            .unwrap()
    };
    assert_eq!(labels(&self_admitted()), [own.label.as_str()]);
    // Marked in the plain list too.
    let listed = fx.store.queue_list(QueueFilter::default()).unwrap();
    let marked: Vec<&str> = listed
        .iter()
        .filter(|i| i.admitted.as_ref().is_some_and(|a| a.self_admitted))
        .map(|i| i.label.as_str())
        .collect();
    assert_eq!(marked, [own.label.as_str()]);
    let list = fx.store.queue_needs_owner().unwrap();
    assert_eq!(labels(&list.self_admitted.recent), [own.label.as_str()]);

    // Thirty days on, finished: off the owner list's recent section, still listed in full.
    fx.claim("ann");
    fx.store
        .queue_done("ann", &own.label, done("abc1234"))
        .unwrap();
    fx.advance(30 * DAY);
    assert_eq!(labels(&self_admitted()), [own.label.as_str()]);
    let list = fx.store.queue_needs_owner().unwrap();
    assert!(list.self_admitted.recent.is_empty());
    assert_eq!(list.self_admitted.total, 1);
}

// T29
#[test]
fn an_admin_release_tells_the_agent() {
    let fx = Fx::new();
    let arch = fx.delegate();
    let item = fx.ready("NOTITLE in the notice");
    fx.claim("ann");
    fx.store.queue_note("ann", &item.label, "half way").unwrap();
    fx.advance(1000);

    // An admitter who is not the claimer, with a reason.
    assert_eq!(
        code(
            fx.store
                .queue_release(&arch, &item.label, ReleaseItem::default())
        ),
        "invalid"
    );
    let released = fx
        .store
        .queue_release(
            &arch,
            &item.label,
            ReleaseItem {
                reason: Some("NOREASON it is needed elsewhere".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(released.state, QueueState::Ready);
    assert_eq!(released.skip_agent.as_deref(), Some("ann"));
    assert!(released.admitted.is_some(), "the approval is kept");
    let unread = fx.unread("ann");
    assert_eq!(unread.len(), 1, "one notice");
    let mail = &unread[0].message;
    assert_eq!(mail.subject, format!("Claim released: {}", item.label));
    assert_eq!(mail.from, "arch", "the delegate that did it");
    assert_eq!(mail.importance, Importance::Normal);
    assert!(!mail.ack_required);
    assert_eq!(mail.thread_id, mail.message_id, "a new thread");
    for needle in [
        fmt_ts(T0 + 1000).as_str(),
        "an admitter released it",
        "kept in the item's history",
        "It will not be offered to you again for the next hour unless an admitter reserves it for you.",
        "(Automatic notice from Another.)",
    ] {
        assert!(
            mail.body.contains(needle),
            "{needle:?} missing from {:?}",
            mail.body
        );
    }
    assert!(!mail.body.contains("NOTITLE") && !mail.body.contains("NOREASON"));
    assert!(
        !mail.body.contains("check that"),
        "the lapse sentence is not for this reason"
    );
    let event = fx.events("queue.released").pop().unwrap();
    assert_eq!(event.actor, "arch");
    assert_eq!(event.payload["reason"], "owner");
    assert_eq!(
        event.payload["previous_claimer_id"],
        fx.agent_id("ann").as_str()
    );
    assert_eq!(event.payload["claimed_at"].as_i64(), Some(T0));
    assert_eq!(event.payload["notice_message_id"], mail.message_id.as_str());
    assert_eq!(event.payload["note"], "NOREASON it is needed elsewhere");
    // The earlier claim and note are still in the history.
    let kinds: Vec<String> = released
        .history
        .unwrap()
        .into_iter()
        .map(|h| h.kind)
        .collect();
    assert_eq!(
        kinds,
        [
            "queue.proposed",
            "queue.admitted",
            "queue.claimed",
            "queue.noted",
            "queue.released"
        ]
    );
    // It is not offered to that agent again, and another agent can take it.
    assert!(fx.store.queue_next("ann").unwrap().claimed.is_none());
    assert_eq!(fx.claim("bob").label, item.label);

    // The owner releases: the system has no agent, so the notice comes from the former
    // claimer itself. It is delivered unread and is announced like any other mail.
    fx.store
        .queue_release(
            &owner(),
            &item.label,
            ReleaseItem {
                reason: Some("stop".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let unread = fx.unread("bob");
    assert_eq!(unread.len(), 1);
    assert_eq!(unread[0].message.from, "bob");
    assert!(unread[0].read_at.is_none());
    let announce = fx.store.unannounced("bob", "", 10).unwrap();
    assert_eq!(announce.count, 1);
    assert_eq!(
        announce.items[0].subject,
        format!("Claim released: {}", item.label)
    );
    assert_eq!(fx.events("queue.released").pop().unwrap().actor, "owner");

    // A notice that cannot be sent: the release stands and the failure is recorded.
    assert_eq!(fx.claim("cy").label, item.label);
    break_mail(&fx.db(), "mail is broken");
    let released = fx
        .store
        .queue_release(
            &owner(),
            &item.label,
            ReleaseItem {
                reason: Some("stop".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(released.state, QueueState::Ready);
    assert!(fx.unread("cy").is_empty());
    let failed = fx
        .events("queue.notify_failed")
        .pop()
        .expect("the failure is recorded");
    assert_eq!(failed.payload["item_id"], item.item_id.as_str());
    assert_eq!(failed.payload["to"], fx.agent_id("cy").as_str());
    assert_eq!(failed.payload["reason"], "owner");
    assert!(failed.payload["error"]
        .as_str()
        .unwrap()
        .contains("mail is broken"));
    let event = fx.events("queue.released").pop().unwrap();
    assert_eq!(event.payload["notice_message_id"], Value::Null);
    assert!(fx.store.queue_audit(None).unwrap().ok);

    // A handoff whose notice fails stands too.
    assert_eq!(fx.claim("bob").label, item.label);
    let handed = fx
        .store
        .queue_release(
            &agent("bob"),
            &item.label,
            ReleaseItem {
                to: Some("cy".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(handed.for_agent.as_deref(), Some("cy"));
    assert_eq!(fx.events("queue.notify_failed").len(), 2);
    assert_eq!(
        fx.events("queue.notify_failed").pop().unwrap().payload["reason"],
        "handoff"
    );
}

// T33
#[test]
fn proposing_a_fast_lane_item_sends_no_mail() {
    let fx = Fx::new();
    fx.propose("ann", "ordinary, older");
    let messages = |fx: &Fx| -> i64 {
        fx.db()
            .query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
            .unwrap()
    };
    let before = messages(&fx);
    for (kind, title) in [
        (QueueKind::Security, "a leak"),
        (QueueKind::DataLoss, "rows vanish"),
    ] {
        fx.advance(1000);
        let item = fx
            .store
            .queue_propose(
                &agent("bob"),
                ProposeItem {
                    title: title.into(),
                    kind,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(item.state, QueueState::Proposed);
        assert_eq!(item.kind, kind);
    }
    assert_eq!(messages(&fx), before, "nothing is mailed");
    let list = fx.store.queue_needs_owner().unwrap();
    assert_eq!(
        labels(&list.decide_now),
        ["Q2", "Q3"],
        "first in the owner list, oldest first"
    );
    assert_eq!(labels(&list.awaiting), ["Q1"]);
    // Until the owner's yes it cannot be claimed.
    assert!(fx.store.queue_next("cy").unwrap().claimed.is_none());
    // The owner admits it at high unless told otherwise.
    let admitted = fx
        .store
        .queue_admit(
            &owner(),
            "Q2",
            AdmitItem {
                scope: Some("M1".into()),
                done_when: Some("the leak is closed".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(admitted.priority, QueuePriority::High);
    let normal = fx
        .store
        .queue_admit(
            &owner(),
            "Q3",
            AdmitItem {
                scope: Some("M1".into()),
                done_when: Some("no rows vanish".into()),
                priority: Some(QueuePriority::Normal),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(normal.priority, QueuePriority::Normal);
    assert_eq!(messages(&fx), before);
    assert_eq!(fx.claim("cy").label, "Q2");
}

/// Every `queue.*` activity line, with a marker in every free-text field: no marker and no
/// payload reaches the feed, and `refs` is the item's id and number or nothing.
#[test]
fn no_item_text_reaches_the_activity_feed() {
    let fx = Fx::new();
    let arch = fx.delegate();
    fx.store.queue_set_limit(&owner(), "ann", 3).unwrap();
    fx.store.queue_set_grace(&owner(), 7200).unwrap();
    let item = fx
        .store
        .queue_propose(
            &agent("ann"),
            ProposeItem {
                title: "MARK_title".into(),
                body: "MARK_body".into(),
                scope: Some("M1".into()),
                done_when: Some("MARK_done".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let rejected = fx.propose("ann", "MARK_title two");
    fx.store
        .queue_reject(&arch, &rejected.label, "MARK_reason")
        .unwrap();
    fx.store
        .queue_admit(&arch, &item.label, AdmitItem::default())
        .unwrap();
    fx.store
        .queue_edit(
            &arch,
            &item.label,
            EditItem {
                done_when: Some("MARK_done again".into()),
                ..Default::default()
            },
        )
        .unwrap();
    fx.claim("ann");
    fx.store
        .queue_note("bob", &item.label, "MARK_note")
        .unwrap();
    fx.store
        .queue_release(
            &agent("ann"),
            &item.label,
            ReleaseItem {
                to: Some("bob".into()),
                note: Some("MARK_handoff".into()),
                ..Default::default()
            },
        )
        .unwrap();
    fx.claim("bob");
    break_mail(&fx.db(), "MARK_error");
    fx.store
        .queue_release(
            &arch,
            &item.label,
            ReleaseItem {
                reason: Some("MARK_reason".into()),
                ..Default::default()
            },
        )
        .unwrap();
    fx.db().execute_batch("DROP TRIGGER break_mail").unwrap();
    fx.claim("cy");
    fx.store
        .queue_done(
            "cy",
            &item.label,
            DoneItem {
                summary: "MARK_summary".into(),
                pointer_kind: PointerKind::File,
                pointer: "MARK_pointer".into(),
            },
        )
        .unwrap();
    fx.store.queue_delegate_remove(&owner(), "arch").unwrap();
    // A kind a later build adds must not fall through to the payload.
    let future_id = seed_event(
        &fx.db(),
        &node_of(&fx),
        T0,
        "queue.future",
        json!({ "item_id": item.item_id, "seq": item.seq, "title": "MARK_future" }),
    );
    // A valid event ID can contain the original privacy marker.
    fx.db()
        .execute(
            "UPDATE events SET event_id = '01M8EKQ3GTMARK20CW00000000' WHERE event_id = ?1",
            [&future_id],
        )
        .unwrap();

    let feed = fx
        .store
        .activity(ActivityQuery {
            since: Some(Since::Time(0)),
            limit: 500,
            kinds: vec![ActivityKind::Queue],
        })
        .unwrap();
    let kinds: BTreeSet<&str> = feed.items.iter().map(|i| i.kind.as_str()).collect();
    assert_eq!(
        kinds.len(),
        15,
        "every queue kind and the unknown one: {kinds:?}"
    );
    for i in &feed.items {
        assert_eq!(i.category, ActivityKind::Queue);
        let text = serde_json::to_string(i).unwrap();
        assert!(!text.contains("MARK_"), "{text}");
        let refs = i.refs.as_object().expect("refs is an object");
        let no_item = [
            "queue.limit_set",
            "queue.delegate_added",
            "queue.delegate_removed",
            "queue.setting_changed",
            "queue.future",
        ]
        .contains(&i.kind.as_str());
        if no_item {
            assert!(refs.is_empty(), "{text}");
        } else {
            assert_eq!(
                refs.keys().collect::<Vec<_>>(),
                ["item_id", "seq"],
                "{text}"
            );
        }
    }
    let line = |kind: &str| -> String {
        feed.items
            .iter()
            .find(|i| i.kind == kind)
            .unwrap_or_else(|| panic!("no {kind}"))
            .summary
            .clone()
    };
    assert_eq!(line("queue.proposed"), "ann proposed Q1 (M1)");
    assert_eq!(line("queue.admitted"), "arch admitted Q1 (M1)");
    assert_eq!(line("queue.claimed"), "ann claimed Q1 (M1)");
    assert_eq!(line("queue.completed"), "cy completed Q1 (M1)");
    assert_eq!(
        line("queue.released"),
        "arch released Q1 (M1): delegate",
        "the authority that acted, not the reason code `owner`"
    );
    assert_eq!(line("queue.future"), "seed: queue.future");
    // The summary of a window and the catch-up digest carry no marker either.
    let summary = fx.store.activity_summary(Some(Since::Time(0))).unwrap();
    assert!(!serde_json::to_string(&summary).unwrap().contains("MARK_"));
    assert_eq!(
        summary.counts.get("queue"),
        Some(&(feed.items.len() as u32))
    );
    let digest = fx.store.whats_new("ann", Some(Since::Time(0)), 20).unwrap();
    assert!(
        digest
            .activity
            .iter()
            .all(|i| !i.kind.starts_with("queue.")),
        "whats_new shows no queue event"
    );
    assert!("queue".parse::<ActivityKind>().is_ok());
}

/// Review R1, P2-1: a write that carries the revision a caller was shown is refused once
/// the item has changed, and goes through when it has not.
#[test]
fn a_shown_write_is_refused_once_the_item_has_changed() {
    let fx = Fx::new();
    let arch = fx.delegate();
    let item = fx.propose("ann", "shown");
    let q = item.label.as_str();
    let edit = |text: &str| EditItem {
        done_when: Some(text.into()),
        ..Default::default()
    };
    let (seen, revision) = fx.store.queue_item_shown(q).unwrap();
    assert_eq!(seen.done_condition.as_deref(), Some("the tests pass"));
    fx.store
        .queue_edit(&arch, q, edit("not what was shown"))
        .unwrap();
    let before = fx.snapshot();
    let stale = |r: Result<QueueItem>| {
        let err = r.unwrap_err();
        assert_eq!(err.code(), "conflict");
        assert!(
            err.to_string()
                .contains(&format!("{q} changed while you were deciding")),
            "{err}"
        );
    };
    stale(
        fx.store
            .queue_admit_shown(&owner(), q, AdmitItem::default(), &revision),
    );
    stale(fx.store.queue_reject_shown(&owner(), q, "no", &revision));
    stale(
        fx.store
            .queue_edit_shown(&owner(), q, edit("again"), &revision),
    );
    assert_eq!(fx.snapshot(), before, "a refused write changes nothing");
    // An edit that changes nothing leaves the revision alone; a note moves it.
    let (_, revision) = fx.store.queue_item_shown(q).unwrap();
    fx.store
        .queue_edit(&arch, q, edit("not what was shown"))
        .unwrap();
    assert_eq!(fx.store.queue_item_shown(q).unwrap().1, revision);
    fx.store.queue_note("bob", q, "read this first").unwrap();
    stale(
        fx.store
            .queue_admit_shown(&owner(), q, AdmitItem::default(), &revision),
    );
    // Unchanged since it was shown: the write goes through.
    let (_, revision) = fx.store.queue_item_shown(q).unwrap();
    let admitted = fx
        .store
        .queue_admit_shown(&owner(), q, AdmitItem::default(), &revision)
        .unwrap();
    assert_eq!(admitted.state, QueueState::Ready);
    // Release: the claim that was shown is gone by the time of the yes.
    fx.claim("ann");
    let (_, revision) = fx.store.queue_item_shown(q).unwrap();
    fx.store
        .queue_release(&agent("ann"), q, ReleaseItem::default())
        .unwrap();
    fx.claim("bob");
    let back = ReleaseItem {
        reason: Some("back".into()),
        ..Default::default()
    };
    stale(
        fx.store
            .queue_release_shown(&owner(), q, back.clone(), &revision),
    );
    assert_eq!(fx.item(q).claim.unwrap().agent, "bob");
    let (_, revision) = fx.store.queue_item_shown(q).unwrap();
    fx.store
        .queue_release_shown(&owner(), q, back, &revision)
        .unwrap();
    // The owner's own release reads as the owner's in the feed.
    let feed = fx
        .store
        .activity(ActivityQuery {
            since: Some(Since::Time(0)),
            limit: 500,
            kinds: vec![ActivityKind::Queue],
        })
        .unwrap();
    assert_eq!(
        feed.items.last().unwrap().summary,
        format!("owner released {q} (M1): owner")
    );
}

/// Review R1, P3-6 and P3-3: one-line text refuses the Unicode line and paragraph
/// separators, and a carriage return is refused with a message that names it.
#[test]
fn one_line_text_refuses_line_separators_and_carriage_returns_are_named() {
    let fx = Fx::new();
    let propose = |title: &str, body: &str, scope: &str| {
        fx.store.queue_propose(
            &agent("ann"),
            ProposeItem {
                title: title.into(),
                body: body.into(),
                scope: Some(scope.into()),
                ..Default::default()
            },
        )
    };
    for separator in ['\u{2028}', '\u{2029}'] {
        let forged = format!("a{separator}Q99  forged");
        let err = propose(&forged, "", "M1").unwrap_err();
        assert_eq!(err.code(), "invalid", "{separator:?} in a title");
        assert!(err.to_string().contains("one line"), "{err}");
        assert_eq!(code(propose("t", "", &forged)), "invalid", "in a scope id");
        let body = format!("para one{separator}para two");
        propose(&format!("body-{separator:?}"), &body, "M1")
            .expect("multi-line text may contain the Unicode line and paragraph separators");
    }
    let item = propose("t", "", "M1").unwrap();
    fx.store
        .queue_admit(
            &owner(),
            &item.label,
            AdmitItem {
                done_when: Some("done".into()),
                ..Default::default()
            },
        )
        .unwrap();
    fx.claim("ann");
    let pointer = DoneItem {
        summary: "ok".into(),
        pointer_kind: PointerKind::File,
        pointer: "docs/a\u{2028}b.md".into(),
    };
    assert_eq!(
        code(fx.store.queue_done("ann", &item.label, pointer)),
        "invalid"
    );

    let err = propose("crlf", "line1\r\nline2\r\n", "M1").unwrap_err();
    assert_eq!(err.code(), "invalid");
    assert!(err.to_string().contains("carriage return"), "{err}");
    assert!(
        !err.to_string().contains("line breaks and tabs are allowed"),
        "{err}"
    );
    // Another control character still gets the general message.
    let err = propose("esc", "a\u{1b}b", "M1").unwrap_err();
    assert!(
        err.to_string().contains("line breaks and tabs are allowed"),
        "{err}"
    );
}

/// The architect's amendment to the skip rule: the agent that gave an item up is skipped
/// for one hour, not for ever, so a node with one agent can take its item back.
#[test]
fn a_skip_ends_one_hour_after_the_release() {
    let fx = Fx::new();
    // One agent with a seat.
    for other in ["bob", "cy", "arch"] {
        fx.store.queue_set_limit(&owner(), other, 0).unwrap();
    }
    let item = fx.ready("the only item");
    assert_eq!(fx.claim("ann").label, item.label);
    let released = fx
        .store
        .queue_release(&agent("ann"), &item.label, ReleaseItem::default())
        .unwrap();
    assert_eq!(released.skip_agent.as_deref(), Some("ann"));
    let offered = |fx: &Fx| fx.store.queue_next("ann").unwrap().claimed.map(|i| i.label);
    assert_eq!(offered(&fx), None, "skipped straight after the release");
    assert_eq!(
        labels(&fx.store.queue_needs_owner().unwrap().nobody_can_take),
        [item.label.as_str()],
        "and listed for the owner meanwhile"
    );
    fx.advance(HOUR - 1);
    assert_eq!(offered(&fx), None, "still skipped just under an hour on");
    fx.advance(1);
    assert!(
        fx.store
            .queue_needs_owner()
            .unwrap()
            .nobody_can_take
            .is_empty(),
        "after an hour the agent can take it again"
    );
    assert_eq!(offered(&fx), Some(item.label.clone()));
    assert_eq!(
        fx.item(&item.label).skip_agent,
        None,
        "a claim clears the skip"
    );

    // The hour runs from the latest release, and an admin release is the same.
    fx.store
        .queue_release(
            &owner(),
            &item.label,
            ReleaseItem {
                reason: Some("not now".into()),
                ..Default::default()
            },
        )
        .unwrap();
    fx.advance(HOUR - 1);
    assert_eq!(offered(&fx), None);
    fx.advance(1);
    assert_eq!(offered(&fx), Some(item.label.clone()));
    // Within the hour another agent is still offered it at once.
    fx.store.queue_set_limit(&owner(), "bob", 1).unwrap();
    fx.store
        .queue_release(&agent("ann"), &item.label, ReleaseItem::default())
        .unwrap();
    assert_eq!(fx.claim("bob").label, item.label);
}

// Results and the lapse rule (scope id M2): session binding, release on the session-end
// hook, release after the grace period, the queue sweep and the lapse notices.

const MINUTE: i64 = 60_000;
const LAPSE_SENTENCE: &str =
    "If you are still working on it, run another queue next and check that";

/// What a hook of `client` passes for its session.
fn hook(client: &str, session: &str) -> SessionInfo {
    SessionInfo {
        client: Some(client.into()),
        session_id: Some(session.into()),
        ..Default::default()
    }
}

/// One `queue_sessions` row: started, seen, ended.
type SessionTimes = (i64, i64, Option<i64>);

impl Fx {
    /// A Claude Code hook of session `session` of `who`.
    fn hooked(&self, who: &str, phase: SessionPhase, session: &str) {
        self.store
            .session(who, phase, hook("claude", session))
            .unwrap();
    }

    fn start(&self, who: &str, session: &str) {
        self.hooked(who, SessionPhase::Start, session);
    }

    fn prompt(&self, who: &str, session: &str) {
        self.hooked(who, SessionPhase::Beat, session);
    }

    fn end(&self, who: &str, session: &str) {
        self.hooked(who, SessionPhase::End, session);
    }

    fn session_row(&self, who: &str, session: &str) -> Option<SessionTimes> {
        use rusqlite::OptionalExtension;
        self.db()
            .query_row(
                "SELECT started_at, seen_at, ended_at FROM queue_sessions
                 WHERE agent_id = ?1 AND session_id = ?2",
                [self.agent_id(who).as_str(), session],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .unwrap()
    }

    fn session_rows(&self) -> i64 {
        self.db()
            .query_row("SELECT count(*) FROM queue_sessions", [], |r| r.get(0))
            .unwrap()
    }

    /// The session the item's claim is bound to, as the row has it.
    fn claim_session(&self, item: &str) -> Option<String> {
        let seq: i64 = item.trim_start_matches('Q').parse().unwrap();
        self.db()
            .query_row(
                "SELECT claim_session FROM queue_items WHERE seq = ?1",
                [seq],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn released(&self) -> Vec<Event> {
        self.events("queue.released")
    }
}

/// The reads that run `Store::sweep` first, as another agent's session would make them.
fn read_everything(fx: &Fx, who: &str) {
    fx.store
        .inbox(
            who,
            InboxQuery {
                unread_only: false,
                urgent_only: false,
                limit: 50,
            },
        )
        .unwrap();
    fx.store.activity(ActivityQuery::default()).unwrap();
    fx.store
        .dashboard_snapshot(DashboardView {
            bodies: false,
            actions: false,
        })
        .unwrap();
    fx.store.sweep().unwrap();
    fx.store.queue_needs_owner().unwrap();
    fx.store.queue_list(QueueFilter::default()).unwrap();
}

// T20
#[test]
fn the_session_end_hook_releases_that_sessions_claims() {
    let fx = Fx::new();
    let item = fx.ready("NOTITLE killed mid-item");
    fx.start("ann", "s1");
    let claimed = fx.claim("ann");
    assert!(claimed.claim.unwrap().session_bound);
    assert_eq!(fx.claim_session(&item.label).as_deref(), Some("s1"));
    assert_eq!(fx.events("queue.claimed")[0].payload["session"], "s1");
    fx.store
        .queue_note("ann", &item.label, "half way, branch q1")
        .unwrap();
    fx.store
        .queue_note("bob", &item.label, "the schema moved")
        .unwrap();
    fx.advance(5 * MINUTE);

    // The session is killed and its end hook runs.
    fx.end("ann", "s1");
    let after = fx.item(&item.label);
    assert_eq!(after.state, QueueState::Ready);
    assert!(after.claim.is_none());
    assert_eq!(after.skip_agent, None, "a lapse sets no skip");
    assert!(after.admitted.is_some(), "the approval is kept");
    assert_eq!(fx.claim_session(&item.label), None);
    let (_, _, ended) = fx.session_row("ann", "s1").unwrap();
    assert_eq!(ended, Some(T0 + 5 * MINUTE));

    let released = fx.released();
    assert_eq!(released.len(), 1);
    let event = &released[0];
    assert_eq!(event.actor, "system");
    assert_eq!(event.payload["reason"], "session_end");
    assert_eq!(
        event.payload["previous_claimer_id"],
        fx.agent_id("ann").as_str()
    );
    assert_eq!(event.payload["claimed_at"].as_i64(), Some(T0));
    assert_eq!(event.payload["note"], Value::Null);
    assert_eq!(event.payload["for_agent_cleared"], Value::Null);
    let ended_event = fx.events("session.ended").pop().unwrap();
    assert_eq!(
        event.payload["evidence"],
        json!({ "session_id": "s1", "event_id": ended_event.event_id })
    );

    // The former claimer is told.
    let unread = fx.unread("ann");
    assert_eq!(unread.len(), 1, "one notice");
    let mail = &unread[0].message;
    assert_eq!(mail.subject, format!("Claim released: {}", item.label));
    assert_eq!(mail.importance, Importance::Normal);
    assert!(!mail.ack_required);
    assert_eq!(mail.thread_id, mail.message_id, "a new thread");
    assert_eq!(event.payload["notice_message_id"], mail.message_id.as_str());
    assert!(!mail.body.contains("NOTITLE") && !mail.body.contains("half way"));

    // Another agent takes it and sees the earlier claim, both notes and the release.
    let taken = fx.claim("bob");
    assert_eq!(taken.label, item.label);
    let history = taken.history.unwrap();
    let kinds: Vec<&str> = history.iter().map(|h| h.kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "queue.proposed",
            "queue.admitted",
            "queue.claimed",
            "queue.noted",
            "queue.noted",
            "queue.released",
            "queue.claimed"
        ]
    );
    assert_eq!(history[2].actor, "ann");
    assert_eq!(history[3].note.as_deref(), Some("half way, branch q1"));
    assert_eq!(history[4].note.as_deref(), Some("the schema moved"));
    assert_eq!(history[5].actor, "system");
    assert_eq!(history[5].detail.as_ref().unwrap()["reason"], "session_end");
    assert!(fx.store.queue_audit(None).unwrap().ok);
    assert_eq!(
        fx.store.queue_audit(None).unwrap().releases["session_end"],
        1
    );
}

// T39
#[test]
fn a_lapsed_reserved_item_can_be_taken_by_another_agent() {
    for (reason, back_to_ann) in [
        ("session_end", false),
        ("session_end", true),
        ("grace", false),
        ("grace", true),
    ] {
        let fx = Fx::new();
        let item = fx.propose("bob", "reserved for ann");
        let item = fx
            .store
            .queue_admit(
                &owner(),
                &item.label,
                AdmitItem {
                    for_agent: Some("ann".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(item.for_agent.as_deref(), Some("ann"));
        assert!(
            fx.store.queue_next("bob").unwrap().claimed.is_none(),
            "reserved: not offered to bob"
        );
        fx.start("ann", "s1");
        assert_eq!(fx.claim("ann").label, item.label);
        if reason == "session_end" {
            fx.end("ann", "s1");
        } else {
            fx.advance(DAY);
            fx.store.queue_sweep().unwrap();
        }
        let after = fx.item(&item.label);
        assert_eq!(after.state, QueueState::Ready, "{reason}");
        assert_eq!(after.for_agent, None, "{reason}: the reservation ended");
        assert_eq!(after.skip_agent, None, "{reason}");
        let event = fx.released().pop().unwrap();
        assert_eq!(event.payload["reason"], reason);
        assert_eq!(
            event.payload["for_agent_cleared"],
            fx.agent_id("ann").as_str()
        );
        let history = after.history.unwrap();
        assert_eq!(
            history.last().unwrap().detail.as_ref().unwrap()["for_agent_cleared"],
            fx.agent_id("ann").as_str()
        );
        let taker = if back_to_ann { "ann" } else { "bob" };
        assert_eq!(fx.claim(taker).label, item.label, "{reason}: {taker}");
        assert!(fx.store.queue_audit(None).unwrap().ok);
    }
}

fn scope_of(fx: &Fx, who: &str) -> TokenScope {
    TokenScope {
        token_id: "tok".into(),
        agent_id: fx.agent_id(who),
        agent: who.into(),
        project_globs: vec!["*".into()],
    }
}

// T21
#[test]
fn only_the_claiming_sessions_end_releases() {
    // A sibling session's end, and an end that names no session or comes from a remote
    // caller, release nothing.
    let fx = Fx::new();
    let item = fx.ready("bound to s1");
    fx.start("ann", "s1");
    assert_eq!(fx.claim("ann").label, item.label);
    assert_eq!(fx.claim_session(&item.label).as_deref(), Some("s1"));
    let still_claimed = |why: &str| {
        let now = fx.item(&item.label);
        assert_eq!(now.state, QueueState::Claimed, "{why}");
        assert_eq!(now.claim.unwrap().agent, "ann", "{why}");
        assert!(fx.released().is_empty(), "{why}");
    };
    fx.start("ann", "s2");
    fx.end("ann", "s2");
    still_claimed("a sibling session ended");
    // `another heartbeat --end` and the MCP `heartbeat` with `end` both call this.
    fx.store.heartbeat("ann", true).unwrap();
    still_claimed("an end with no session id");
    fx.store
        .session(
            "ann",
            SessionPhase::End,
            SessionInfo {
                client: Some("claude".into()),
                ..Default::default()
            },
        )
        .unwrap();
    still_claimed("a hook end with no session id");
    fx.store
        .session_scoped(
            "ann",
            SessionPhase::End,
            hook("claude", "s1"),
            Some(&scope_of(&fx, "ann")),
        )
        .unwrap();
    still_claimed("a remote caller's end, even with the session id");
    fx.end("bob", "s1");
    still_claimed("another agent's session with the same id");
    fx.store
        .session("ann", SessionPhase::End, hook("codex", "s1"))
        .unwrap();
    still_claimed("a client whose session ids are not relied on");
    assert!(fx.unread("ann").is_empty());
    fx.end("ann", "s1");
    assert_eq!(fx.item(&item.label).state, QueueState::Ready);

    // Two sessions open under one name: the claim is bound to neither, and neither end
    // releases it.
    let fx = Fx::new();
    let item = fx.ready("two sessions open");
    fx.start("ann", "s1");
    fx.start("ann", "s2");
    let claimed = fx.claim("ann");
    assert!(!claimed.claim.unwrap().session_bound);
    fx.end("ann", "s1");
    assert_eq!(fx.item(&item.label).state, QueueState::Claimed);
    fx.end("ann", "s2");
    assert_eq!(fx.item(&item.label).state, QueueState::Claimed);
    assert!(fx.released().is_empty());

    // An end, then a start with a new id (as after a clear): only the old id's claims go.
    let fx = Fx::new();
    fx.store.queue_set_limit(&owner(), "ann", 2).unwrap();
    let first = fx.ready("claimed by the old session");
    fx.start("ann", "old");
    assert_eq!(fx.claim("ann").label, first.label);
    fx.end("ann", "old");
    assert_eq!(fx.item(&first.label).state, QueueState::Ready);
    fx.start("ann", "new");
    assert_eq!(fx.claim("ann").label, first.label);
    assert_eq!(fx.claim_session(&first.label).as_deref(), Some("new"));
    let second = fx.ready("claimed by the new session");
    assert_eq!(fx.claim("ann").label, second.label);
    // The old session's end hook arriving again releases nothing of the new session's.
    fx.end("ann", "old");
    assert_eq!(fx.item(&first.label).state, QueueState::Claimed);
    assert_eq!(fx.item(&second.label).state, QueueState::Claimed);
    assert_eq!(fx.released().len(), 1);
    fx.end("ann", "new");
    assert_eq!(fx.item(&first.label).state, QueueState::Ready);
    assert_eq!(fx.item(&second.label).state, QueueState::Ready);
    assert_eq!(fx.released().len(), 3);
    assert_eq!(fx.unread("ann").len(), 3, "one notice for each release");
    assert!(fx.store.queue_audit(None).unwrap().ok);
}

// T36
#[test]
fn binding_is_never_a_guess() {
    // A reused id opens its row again and keeps when it started.
    let fx = Fx::new();
    fx.start("ann", "s1");
    assert_eq!(fx.session_row("ann", "s1"), Some((T0, T0, None)));
    fx.advance(MINUTE);
    fx.end("ann", "s1");
    assert_eq!(
        fx.session_row("ann", "s1"),
        Some((T0, T0, Some(T0 + MINUTE)))
    );
    fx.advance(MINUTE);
    fx.start("ann", "s1");
    assert_eq!(
        fx.session_row("ann", "s1"),
        Some((T0, T0 + 2 * MINUTE, None)),
        "a resumed session that reuses its id is open again"
    );
    // A beat after a tool call is a sign of that session, and so is a prompt.
    fx.advance(MINUTE);
    fx.store.keepalive("ann", &hook("claude", "s1")).unwrap();
    assert_eq!(fx.session_row("ann", "s1").unwrap().1, T0 + 3 * MINUTE);
    fx.advance(MINUTE);
    fx.prompt("ann", "s1");
    assert_eq!(fx.session_row("ann", "s1").unwrap().1, T0 + 4 * MINUTE);
    let item = fx.ready("one fresh session");
    assert_eq!(fx.claim("ann").label, item.label);
    assert_eq!(fx.claim_session(&item.label).as_deref(), Some("s1"));

    // With a sibling at its prompt every hour, a session that only beats after tools
    // stays fresh for longer than the grace period, so the name is never taken for one
    // session and the claim is bound to neither.
    let fx = Fx::new();
    fx.start("ann", "tools");
    fx.start("ann", "prompts");
    for _ in 0..30 {
        fx.advance(HOUR);
        fx.store.keepalive("ann", &hook("claude", "tools")).unwrap();
        fx.prompt("ann", "prompts");
    }
    let item = fx.ready("two fresh sessions");
    let claimed = fx.claim("ann");
    assert_eq!(claimed.label, item.label);
    assert!(!claimed.claim.unwrap().session_bound);
    assert_eq!(fx.claim_session(&item.label), None);
    assert_eq!(
        fx.events("queue.claimed")[0].payload["session"],
        Value::Null
    );

    // A client whose session ids are not relied on at all four hook events writes no row,
    // and neither does a call with no session id or a remote caller.
    let fx = Fx::new();
    for client in ["codex", "grok", "cursor", "Claude", ""] {
        fx.store
            .session("ann", SessionPhase::Start, hook(client, "x1"))
            .unwrap();
        fx.store
            .session("ann", SessionPhase::Beat, hook(client, "x1"))
            .unwrap();
        fx.store.keepalive("ann", &hook(client, "x1")).unwrap();
    }
    fx.store
        .session(
            "ann",
            SessionPhase::Start,
            SessionInfo {
                session_id: Some("x1".into()),
                ..Default::default()
            },
        )
        .unwrap();
    fx.store
        .session("ann", SessionPhase::Start, hook("claude", ""))
        .unwrap();
    fx.store.heartbeat("ann", false).unwrap();
    fx.store.keepalive("ann", &SessionInfo::default()).unwrap();
    fx.store
        .session_scoped(
            "ann",
            SessionPhase::Start,
            hook("claude", "x1"),
            Some(&scope_of(&fx, "ann")),
        )
        .unwrap();
    assert_eq!(fx.session_rows(), 0);
    let item = fx.ready("no session row");
    assert!(!fx.claim("ann").claim.unwrap().session_bound);
    fx.store
        .session("ann", SessionPhase::End, hook("codex", "x1"))
        .unwrap();
    assert_eq!(fx.item(&item.label).state, QueueState::Claimed);
    // The session id still goes into the session events as it came.
    assert_eq!(fx.events("session.started")[0].payload["session_id"], "x1");
    assert_eq!(fx.events("session.started")[0].payload["client"], "codex");

    // The limit of binding by name, pinned: a caller with no session row of its own that
    // claims under a name with exactly one fresh session is bound to that session, and
    // that session's end releases the claim.
    let fx = Fx::new();
    fx.start("ann", "hooked");
    let item = fx.ready("claimed by a caller with no hooks");
    let by_mcp = fx.store.queue_next("ann").unwrap().claimed.unwrap();
    assert!(by_mcp.claim.unwrap().session_bound);
    assert_eq!(fx.claim_session(&item.label).as_deref(), Some("hooked"));
    fx.end("ann", "hooked");
    assert_eq!(fx.item(&item.label).state, QueueState::Ready);

    // Rows not seen for 30 days are deleted by the queue sweep; a newer one stays.
    let fx = Fx::new();
    fx.start("ann", "old");
    fx.start("bob", "ended");
    fx.end("bob", "ended");
    fx.advance(30 * DAY - 1);
    fx.start("cy", "new");
    fx.store.queue_sweep().unwrap();
    assert_eq!(fx.session_rows(), 3, "not yet 30 days");
    fx.advance(1);
    let swept = fx.store.queue_sweep().unwrap();
    assert_eq!(swept.forgotten_sessions, 2);
    assert_eq!(fx.session_rows(), 1);
    assert!(fx.session_row("cy", "new").is_some());
}

// T38
#[test]
fn a_stale_session_row_is_ignored_for_binding() {
    let fx = Fx::new();
    fx.store.queue_set_limit(&owner(), "ann", 2).unwrap();
    let first = fx.ready("claimed before the crash");
    fx.start("ann", "dead");
    assert_eq!(fx.claim("ann").label, first.label);
    // The session crashes: no end hook. A grace period of silence follows, with no sweep.
    fx.advance(DAY);
    // The session restarted under the same name is the only fresh row.
    fx.start("ann", "restarted");
    assert_eq!(fx.session_row("ann", "dead"), Some((T0, T0, None)));
    let second = fx.ready("claimed after the restart");
    assert_eq!(fx.claim("ann").label, second.label);
    assert_eq!(
        fx.claim_session(&second.label).as_deref(),
        Some("restarted"),
        "a stale row does not make the name ambiguous"
    );
    // The dead session's claim still stands, bound to the dead session.
    assert_eq!(fx.item(&first.label).state, QueueState::Claimed);
    assert_eq!(fx.claim_session(&first.label).as_deref(), Some("dead"));
    assert!(fx.released().is_empty());
    // Only the new session's claim goes at its end.
    fx.end("ann", "restarted");
    assert_eq!(fx.item(&second.label).state, QueueState::Ready);
    assert_eq!(fx.item(&first.label).state, QueueState::Claimed);

    // One millisecond inside the grace period the old row is still fresh.
    let fx = Fx::new();
    fx.start("ann", "quiet");
    fx.advance(DAY - 1);
    fx.start("ann", "second");
    let item = fx.ready("two fresh rows");
    fx.claim("ann");
    assert_eq!(fx.claim_session(&item.label), None);

    // With no fresh row at all the claim is unbound.
    let fx = Fx::new();
    fx.start("cy", "long ago");
    fx.advance(DAY);
    let item = fx.ready("no fresh row");
    let claimed = fx.claim("cy");
    assert!(!claimed.claim.unwrap().session_bound);
    assert_eq!(fx.claim_session(&item.label), None);
    // An ended row is not fresh either.
    let fx = Fx::new();
    fx.start("cy", "over");
    fx.end("cy", "over");
    let item = fx.ready("only an ended row");
    fx.claim("cy");
    assert_eq!(fx.claim_session(&item.label), None);
}

/// Everything that runs between an agent's own signs of life: every sweep, the other
/// agents' hooks, heartbeats and `next`, and the reads.
fn everything_else_runs(fx: &Fx) {
    fx.store.sweep().unwrap();
    fx.store.queue_sweep().unwrap();
    fx.prompt("bob", "bob-session");
    fx.store.heartbeat("cy", false).unwrap();
    fx.store.queue_next("bob").unwrap();
    read_everything(fx, "bob");
}

// T22
#[test]
fn an_agent_silent_for_an_hour_keeps_its_claim() {
    let fx = Fx::new();
    let project = fx.dir.path().join("shop");
    std::fs::create_dir_all(&project).unwrap();
    let project = fx
        .store
        .ensure_project("shop", project.to_str().unwrap())
        .unwrap()
        .abs_path;
    let item = fx.ready("a long think");
    fx.start("ann", "s1");
    assert_eq!(fx.claim("ann").label, item.label);
    fx.store
        .acquire_holds(AcquireHolds {
            agent: "ann".into(),
            project: project.clone(),
            paths: vec!["src/**".into()],
            ttl_seconds: Some(600),
            exclusive: true,
            reason: None,
            repo_wide: false,
        })
        .unwrap();
    let live_holds = || {
        fx.store
            .holds(HoldFilter {
                project: Some(project.clone()),
                agent: Some("ann".into()),
            })
            .unwrap()
            .len()
    };
    assert_eq!(live_holds(), 1);
    let stands = |why: &str| {
        let now = fx.item(&item.label);
        assert_eq!(now.state, QueueState::Claimed, "{why}");
        assert_eq!(now.claim.unwrap().agent, "ann", "{why}");
        assert!(fx.released().is_empty(), "{why}");
        assert!(fx
            .unread("ann")
            .iter()
            .all(|m| !m.message.subject.starts_with("Claim released")));
    };

    // Silent but alive for an hour: the hold has expired, the claim has not.
    fx.advance(HOUR);
    everything_else_runs(&fx);
    assert_eq!(live_holds(), 0, "the hold expired");
    stands("an hour of silence");
    // Marked offline, a heartbeat missed, a wake that did not start work: none releases.
    fx.db()
        .execute(
            "UPDATE agents SET status = 'offline' WHERE agent_id = ?1",
            [fx.agent_id("ann")],
        )
        .unwrap();
    seed_event(
        &fx.db(),
        &node_of(&fx),
        T0 + HOUR,
        "transport.wake",
        json!({ "agent_id": fx.agent_id("ann"), "session_id": "s1", "outcome": "not_started" }),
    );
    everything_else_runs(&fx);
    stands("offline, with a failed wake");

    // One minute short of the grace period.
    fx.advance(22 * HOUR + 59 * MINUTE);
    everything_else_runs(&fx);
    stands("23 hours 59 minutes of silence");
    fx.advance(MINUTE - 1);
    everything_else_runs(&fx);
    stands("one millisecond short of the grace period");
}

// T23
#[test]
fn a_beat_moves_the_grace_deadline() {
    let fx = Fx::new();
    let item = fx.ready("a beat at hour 23");
    fx.start("ann", "s1");
    fx.claim("ann");
    fx.advance(23 * HOUR);
    fx.prompt("ann", "s1");
    fx.advance(HOUR + MINUTE);
    everything_else_runs(&fx);
    assert_eq!(
        fx.item(&item.label).state,
        QueueState::Claimed,
        "past hour 24, an hour after the beat"
    );
    // A beat after a tool call counts as well.
    fx.advance(22 * HOUR);
    fx.store.keepalive("ann", &hook("claude", "s1")).unwrap();
    fx.advance(23 * HOUR + 59 * MINUTE);
    everything_else_runs(&fx);
    assert_eq!(fx.item(&item.label).state, QueueState::Claimed);
    // The deadline is 24 hours after the last beat.
    fx.advance(MINUTE);
    fx.store.queue_sweep().unwrap();
    assert_eq!(fx.item(&item.label).state, QueueState::Ready);
    let event = fx.released().pop().unwrap();
    assert_eq!(event.payload["reason"], "grace");
    assert_eq!(
        event.payload["evidence"]["last_sign_of_life"].as_i64(),
        Some(T0 + 23 * HOUR + HOUR + MINUTE + 22 * HOUR)
    );
}

// T24
#[test]
fn grace_releases_after_24_hours_of_silence() {
    // Each of the three callers of the queue sweep releases an overdue claim.
    for caller in ["sweep", "hook", "heartbeat", "next"] {
        let fx = Fx::new();
        let item = fx.ready("left behind");
        // No session: a killed pane with no end hook, or a caller with no hooks.
        assert_eq!(fx.claim("ann").label, item.label);
        fx.store
            .queue_note("ann", &item.label, "got this far")
            .unwrap();
        fx.advance(DAY - 1);
        everything_else_runs(&fx);
        assert_eq!(fx.item(&item.label).state, QueueState::Claimed, "{caller}");
        fx.advance(1);
        // The reads that run the hold sweep release nothing.
        read_everything(&fx, "bob");
        assert_eq!(fx.item(&item.label).state, QueueState::Claimed, "{caller}");
        assert!(fx.released().is_empty(), "{caller}");
        match caller {
            "sweep" => {
                let swept = fx.store.queue_sweep().unwrap();
                assert_eq!(swept.released_claims, [item.label.as_str()]);
            }
            "hook" => fx.prompt("bob", "bob-session"),
            "heartbeat" => {
                fx.store.heartbeat("bob", false).unwrap();
            }
            _ => {
                // Another agent's `next` releases the claim and is handed the item.
                let taken = fx.claim("bob");
                assert_eq!(taken.label, item.label);
                let kinds: Vec<String> =
                    taken.history.unwrap().into_iter().map(|h| h.kind).collect();
                assert_eq!(
                    kinds,
                    [
                        "queue.proposed",
                        "queue.admitted",
                        "queue.claimed",
                        "queue.noted",
                        "queue.released",
                        "queue.claimed"
                    ]
                );
            }
        }
        let released = fx.released();
        assert_eq!(released.len(), 1, "{caller}");
        let event = &released[0];
        assert_eq!(event.actor, "system", "{caller}");
        assert_eq!(event.payload["reason"], "grace", "{caller}");
        assert_eq!(
            event.payload["previous_claimer_id"],
            fx.agent_id("ann").as_str()
        );
        assert_eq!(
            event.payload["evidence"],
            json!({ "last_sign_of_life": T0, "grace_seconds": 86_400 }),
            "{caller}"
        );
        if caller != "next" {
            let after = fx.item(&item.label);
            assert_eq!(after.state, QueueState::Ready, "{caller}");
            assert_eq!(after.skip_agent, None);
        }
        // A second sweep finds nothing more.
        assert!(fx.store.queue_sweep().unwrap().released_claims.is_empty());
        assert!(fx.store.queue_audit(None).unwrap().ok, "{caller}");
        assert_eq!(fx.store.queue_audit(None).unwrap().releases["grace"], 1);
    }

    // The grace period is the owner's setting, bounded, and the sweep follows it.
    let fx = Fx::new();
    let arch = fx.delegate();
    assert_eq!(
        code(fx.store.queue_set_grace(&agent("ann"), 7200)),
        "forbidden"
    );
    assert_eq!(code(fx.store.queue_set_grace(&arch, 7200)), "forbidden");
    assert_eq!(code(fx.store.queue_set_grace(&owner(), 7199)), "invalid");
    assert_eq!(
        code(fx.store.queue_set_grace(&owner(), 30 * 86_400 + 1)),
        "invalid"
    );
    fx.store.queue_set_grace(&owner(), 7200).unwrap();
    let item = fx.ready("a two hour grace period");
    fx.claim("ann");
    fx.advance(2 * HOUR - 1);
    fx.store.queue_sweep().unwrap();
    assert_eq!(fx.item(&item.label).state, QueueState::Claimed);
    fx.advance(1);
    fx.store.queue_sweep().unwrap();
    assert_eq!(fx.item(&item.label).state, QueueState::Ready);
    assert_eq!(
        fx.released()[0].payload["evidence"]["grace_seconds"].as_i64(),
        Some(7200)
    );
}

// T35
#[test]
fn an_agents_own_return_does_not_release_its_claim() {
    for way in ["prompt", "start", "end", "next", "inbox", "heartbeat"] {
        let fx = Fx::new();
        let item = fx.ready("away for 25 hours");
        fx.start("ann", "s1");
        fx.claim("ann");
        // 25 hours with no sweep at all, then the claimer comes back.
        fx.advance(25 * HOUR);
        match way {
            "prompt" => fx.prompt("ann", "s1"),
            "start" => fx.start("ann", "s2"),
            // The end of another session of the name: the claim's own would release it.
            "end" => fx.end("ann", "s2"),
            "next" => {
                let next = fx.store.queue_next("ann").unwrap();
                assert_eq!(next.refused.unwrap().held, [item.label.as_str()]);
            }
            "heartbeat" => {
                fx.store.heartbeat("ann", false).unwrap();
            }
            _ => read_everything(&fx, "ann"),
        }
        let now = fx.item(&item.label);
        assert_eq!(now.state, QueueState::Claimed, "{way}");
        assert_eq!(now.claim.unwrap().agent, "ann", "{way}");
        assert!(fx.released().is_empty(), "{way}");
        if way != "inbox" {
            // The return is a sign of life: the next sweep keeps the claim too.
            fx.store.queue_sweep().unwrap();
            assert_eq!(fx.item(&item.label).state, QueueState::Claimed, "{way}");
        }
    }
}

// T25
#[test]
fn every_lapse_tells_the_agent() {
    for reason in ["session_end", "grace"] {
        let fx = Fx::new();
        let item = fx.ready("NOTITLE in the notice");
        fx.start("ann", "s1");
        fx.claim("ann");
        fx.store
            .queue_note("ann", &item.label, "NONOTE in the notice")
            .unwrap();
        let (at, words) = if reason == "session_end" {
            fx.advance(MINUTE);
            fx.end("ann", "s1");
            (T0 + MINUTE, "the session that claimed it ended")
        } else {
            fx.advance(DAY);
            fx.store.queue_sweep().unwrap();
            (T0 + DAY, "nothing was heard from you for the grace period")
        };
        let unread = fx.unread("ann");
        assert_eq!(unread.len(), 1, "{reason}: one notice");
        let mail = &unread[0].message;
        assert_eq!(mail.subject, format!("Claim released: {}", item.label));
        assert_eq!(mail.from, "ann", "the system has no agent");
        assert_eq!(mail.importance, Importance::Normal);
        assert!(!mail.ack_required);
        assert_eq!(mail.thread_id, mail.message_id, "a new thread");
        let comes_back = format!(
            "{LAPSE_SENTENCE} {} comes back; another agent may take it first.",
            item.label
        );
        for needle in [
            fmt_ts(at).as_str(),
            words,
            "kept in the item's history",
            comes_back.as_str(),
            "(Automatic notice from Another.)",
        ] {
            assert!(
                mail.body.contains(needle),
                "{reason}: {needle:?} missing from {:?}",
                mail.body
            );
        }
        assert!(!mail.body.contains("NOTITLE") && !mail.body.contains("NONOTE"));
        assert!(!mail.body.contains("s1"), "no session id in the mail");
        assert!(
            !mail.body.contains("for the next hour"),
            "{reason}: a lapse sets no skip"
        );
        let event = fx.released().pop().unwrap();
        assert_eq!(event.payload["notice_message_id"], mail.message_id.as_str());
        assert!(fx.events("queue.notify_failed").is_empty());
    }

    // A notice that cannot be sent leaves the release standing.
    for reason in ["session_end", "grace"] {
        let fx = Fx::new();
        let item = fx.ready("the mail is broken");
        fx.start("cy", "s1");
        fx.claim("cy");
        break_mail(&fx.db(), "mail is broken");
        if reason == "session_end" {
            fx.end("cy", "s1");
        } else {
            fx.advance(DAY);
            fx.store.queue_sweep().unwrap();
        }
        assert_eq!(fx.item(&item.label).state, QueueState::Ready, "{reason}");
        assert!(fx.unread("cy").is_empty());
        let failed = fx
            .events("queue.notify_failed")
            .pop()
            .expect("the failure is recorded");
        assert_eq!(failed.payload["item_id"], item.item_id.as_str());
        assert_eq!(failed.payload["to"], fx.agent_id("cy").as_str());
        assert_eq!(failed.payload["reason"], reason);
        assert!(failed.payload["error"]
            .as_str()
            .unwrap()
            .contains("mail is broken"));
        let event = fx.released().pop().unwrap();
        assert_eq!(event.payload["reason"], reason);
        assert_eq!(event.payload["notice_message_id"], Value::Null);
        if reason == "session_end" {
            assert_eq!(
                fx.events("session.ended").len(),
                1,
                "the session still ends"
            );
        }
        let list = fx.store.queue_needs_owner().unwrap();
        assert_eq!(list.failed_notices.len(), 1);
        assert!(fx.store.queue_audit(None).unwrap().ok);
    }
}

// T37
#[test]
fn quiet_claim_reasons() {
    /// The quiet claims as (label, reason), in list order.
    fn reasons(fx: &Fx) -> Vec<(String, String)> {
        fx.store
            .queue_needs_owner()
            .unwrap()
            .quiet_claims
            .into_iter()
            .map(|q| (q.item.label, q.reason))
            .collect()
    }

    let fx = Fx::new();
    // Bound to a session that goes quiet while a sibling keeps the name active.
    let stale = fx.ready("bound, its session quiet");
    fx.start("ann", "quiet");
    assert_eq!(fx.claim("ann").label, stale.label);
    // Bound to a session that stays active.
    let live = fx.ready("bound, its session active");
    fx.start("bob", "busy");
    assert_eq!(fx.claim("bob").label, live.label);
    // Unbound: no session behind the claim.
    let unbound = fx.ready("unbound");
    assert_eq!(fx.claim("cy").label, unbound.label);

    let before = fx.snapshot();
    let list = fx.store.queue_needs_owner().unwrap();
    assert_eq!(fx.snapshot(), before, "the read writes nothing");
    assert!(
        list.quiet_claims.is_empty(),
        "a fresh unbound claim is not listed: {:?}",
        list.quiet_claims
    );

    // What a healthy shared name does lists nothing: a sibling's `heartbeat --end`, a
    // sibling that sits idle, and a sibling whose pane the transport no longer lists.
    fx.start("cy", "idle sibling");
    fx.store
        .stamp_transport_session("cy", "gone sibling", "herdr", "w1:p1", "claude", "/work")
        .unwrap();
    fx.store
        .observe_transport_sessions("herdr", Ok(&[]), None)
        .unwrap();
    fx.advance(2 * HOUR);
    fx.store.heartbeat("cy", true).unwrap();
    fx.start("ann", "sibling");
    fx.prompt("bob", "busy");
    let list = fx.store.queue_needs_owner().unwrap();
    assert_eq!(
        list.quiet_claims
            .iter()
            .map(|q| (q.item.label.as_str(), q.reason.as_str()))
            .collect::<Vec<_>>(),
        [(stale.label.as_str(), "session_quiet")],
        "only the bound claim whose own session is quiet"
    );
    let q = &list.quiet_claims[0];
    assert!(!q.unbound);
    assert_eq!(q.session_last_seen_at.as_deref(), Some(fmt_ts(T0).as_str()));
    assert_eq!(q.session_quiet_for_seconds, Some(7200));
    assert_eq!(q.quiet_for_seconds, 0, "the name itself is active");
    assert_eq!(q.grace_ends_at, fmt_ts(T0 + 2 * HOUR + DAY));
    let v = serde_json::to_value(q).unwrap();
    assert_eq!(v["reason"], "session_quiet");
    assert_eq!(v["session_quiet_for_seconds"], 7200);

    // A quiet name comes first of the reasons, for a bound claim and an unbound one.
    fx.advance(HOUR);
    let list = fx.store.queue_needs_owner().unwrap();
    let by_label = |label: &str| list.quiet_claims.iter().find(|q| q.item.label == label);
    let q = by_label(&live.label).unwrap();
    assert_eq!(q.reason, "name_quiet");
    assert!(!q.unbound);
    assert_eq!(q.session_quiet_for_seconds, Some(3600));
    assert_eq!(q.quiet_for_seconds, 3600);
    let q = by_label(&stale.label).unwrap();
    assert_eq!(q.reason, "name_quiet", "reason 1 before reason 2");
    let q = by_label(&unbound.label).unwrap();
    assert_eq!(q.reason, "name_quiet");
    assert!(q.unbound);
    assert_eq!(q.session_quiet_for_seconds, None);
    // A bound claim whose session row is gone is treated as unbound.
    fx.db()
        .execute("DELETE FROM queue_sessions WHERE session_id = 'busy'", [])
        .unwrap();
    let list = fx.store.queue_needs_owner().unwrap();
    let q = list
        .quiet_claims
        .iter()
        .find(|q| q.item.label == live.label)
        .unwrap();
    assert!(q.unbound);
    assert_eq!(q.session_last_seen_at, None);
    assert!(fx.released().is_empty(), "listing releases nothing");

    // An unbound claim under a name that stays active: listed once it is one grace period
    // old, and not one millisecond earlier.
    let fx = Fx::new();
    let old = fx.ready("unbound under an active name");
    assert_eq!(fx.claim("cy").label, old.label);
    fx.advance(DAY - 1);
    fx.store.heartbeat("cy", false).unwrap();
    assert!(reasons(&fx).is_empty(), "{:?}", reasons(&fx));
    fx.advance(1);
    assert_eq!(
        reasons(&fx),
        [(old.label.clone(), "old_unbound".to_string())]
    );
    let list = fx.store.queue_needs_owner().unwrap();
    assert!(list.quiet_claims[0].unbound);
    assert_eq!(list.quiet_claims[0].quiet_for_seconds, 0);
    // A sign of life does not clear it.
    fx.store.heartbeat("cy", false).unwrap();
    assert_eq!(
        reasons(&fx),
        [(old.label.clone(), "old_unbound".to_string())]
    );
    // With nothing heard for an hour it is a quiet name first.
    fx.advance(HOUR);
    assert_eq!(
        reasons(&fx),
        [(old.label.clone(), "name_quiet".to_string())]
    );
    assert!(fx.released().is_empty(), "listing releases nothing");

    // The grace period is the owner's setting: at 2 hours the claim is listed after 2 hours.
    let fx = Fx::new();
    fx.store.queue_set_grace(&owner(), 2 * 3600).unwrap();
    let short = fx.ready("unbound, a short grace period");
    assert_eq!(fx.claim("cy").label, short.label);
    fx.advance(2 * HOUR - 1);
    fx.store.heartbeat("cy", false).unwrap();
    assert!(reasons(&fx).is_empty(), "{:?}", reasons(&fx));
    fx.advance(1);
    assert_eq!(reasons(&fx), [(short.label, "old_unbound".to_string())]);
}

#[test]
fn session_bound_follows_the_session_row() {
    let fx = Fx::new();
    let item = fx.ready("bound to a session");
    fx.start("ann", "s1");
    let claimed = fx.claim("ann");
    assert!(claimed.claim.unwrap().session_bound);
    let claims = fx.store.queue_claims().unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(
        (
            claims[0].item.as_str(),
            claims[0].agent.as_str(),
            claims[0].session.as_deref()
        ),
        (item.label.as_str(), "ann", Some("s1"))
    );

    fx.db()
        .execute("DELETE FROM queue_sessions WHERE session_id = 's1'", [])
        .unwrap();
    assert_eq!(fx.claim_session(&item.label).as_deref(), Some("s1"));
    assert!(
        !fx.item(&item.label).claim.unwrap().session_bound,
        "the column alone binds nothing"
    );
    let claims = fx.store.queue_claims().unwrap();
    assert_eq!(claims[0].session, None);
}

#[test]
fn an_overlong_session_id_binds_nothing() {
    let fx = Fx::new();
    let item = fx.ready("claimed by a session with a crafted id");
    fx.start("ann", &"x".repeat(257));
    assert_eq!(fx.session_rows(), 0, "257 bytes make no session row");
    assert!(!fx.claim("ann").claim.unwrap().session_bound);
    assert_eq!(fx.claim_session(&item.label), None);

    let fx = Fx::new();
    let item = fx.ready("claimed by a session with the longest id");
    let longest = "x".repeat(256);
    fx.start("ann", &longest);
    assert_eq!(fx.session_rows(), 1);
    assert!(fx.claim("ann").claim.unwrap().session_bound);
    assert_eq!(fx.claim_session(&item.label), Some(longest));
}

#[test]
fn claims_past_one_hundred_are_not_lost() {
    let fx = Fx::new();
    let mut names = Vec::new();
    for n in 0..8 {
        let name = format!("worker{n}");
        register(&fx.store, &name);
        fx.store
            .queue_set_limit(&owner(), &name, MAX_QUEUE_LIMIT)
            .unwrap();
        names.push(name);
    }
    for n in 0..150 {
        fx.ready(&format!("item {n}"));
    }
    for n in 0..150 {
        fx.claim(&names[n % names.len()]);
    }
    let claims = fx.store.queue_claims().unwrap();
    assert_eq!(claims.len(), 150);
    let distinct: BTreeSet<&str> = claims.iter().map(|c| c.item.as_str()).collect();
    assert_eq!(distinct.len(), 150);
    assert!(claims.iter().all(|c| c.session.is_none()));
}

// The read behind the hook packs' queue lines (scope id M6, D96): what `next` would give an
// agent now, without writing.

/// A small repeatable generator, so a failing state can be found again by its seed.
struct Gen(u64);

impl Gen {
    fn below(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }
}

// Q10 T1
#[test]
fn offer_and_next_agree() {
    let (mut offered, mut silent) = (0, 0);
    for seed in 0..120u64 {
        let mut gen = Gen(seed);
        let fx = Fx::new();
        // Room for the setup; ann's real limit is set at the end.
        fx.store.queue_set_limit(&owner(), "ann", 5).unwrap();
        fx.store.queue_set_limit(&owner(), "bob", 3).unwrap();
        // Claims of the name and of others, taken while nothing else is ready.
        for (who, most) in [("ann", 3), ("bob", 2)] {
            for n in 0..gen.below(most + 1) {
                fx.ready(&format!("held by {who} {n}"));
                fx.claim(who);
            }
        }
        // An item ann gave back: skipped for ann for one hour from the release.
        if gen.below(2) == 0 {
            let item = fx.ready("given back by ann");
            fx.claim("ann");
            fx.store
                .queue_release(&agent("ann"), &item.label, ReleaseItem::default())
                .unwrap();
            fx.advance(if gen.below(2) == 0 {
                HOUR / 2
            } else {
                HOUR + 1
            });
        }
        // Ready items: both priorities, reserved for ann, for another agent, or for nobody.
        for n in 0..gen.below(4) {
            let item = fx.propose("ann", &format!("ready {n}"));
            let admit = AdmitItem {
                priority: (gen.below(2) == 0).then_some(QueuePriority::High),
                for_agent: match gen.below(4) {
                    0 => Some("ann".to_string()),
                    1 => Some("cy".to_string()),
                    _ => None,
                },
                ..Default::default()
            };
            fx.store.queue_admit(&owner(), &item.label, admit).unwrap();
            fx.advance(1 + gen.below(5) as i64);
            // An item of another node is never handed out here.
            if gen.below(5) == 0 {
                let db = fx.db();
                let foreign = foreign_node(&db);
                db.execute(
                    "UPDATE queue_items SET node_id = ?1 WHERE item_id = ?2",
                    [&foreign, &item.item_id],
                )
                .unwrap();
            }
        }
        match gen.below(5) {
            4 => {
                // No limit of its own: the default applies.
                fx.db()
                    .execute(
                        "DELETE FROM queue_limits WHERE agent_id = ?1",
                        [fx.agent_id("ann")],
                    )
                    .unwrap();
            }
            limit => {
                fx.store
                    .queue_set_limit(&owner(), "ann", limit as i64)
                    .unwrap();
            }
        }

        let offer = fx.store.queue_offer("ann").unwrap();
        let next = fx.store.queue_next("ann").unwrap();
        assert_eq!(
            offer.ready.is_some(),
            next.claimed.is_some(),
            "seed {seed}: the offer was {offer:?}, next gave {next:?}"
        );
        match (&offer.ready, &next.claimed) {
            (Some(mark), Some(item)) => {
                offered += 1;
                assert!(
                    mark.starts_with(&format!("{}:", item.item_id)),
                    "seed {seed}: the mark {mark} names another item than {}",
                    item.label
                );
            }
            _ => silent += 1,
        }
        if let Some(refused) = &next.refused {
            assert_eq!(offer.limit, refused.limit, "seed {seed}");
            let held: Vec<&str> = offer.held.iter().map(|h| h.label.as_str()).collect();
            assert_eq!(held, refused.held, "seed {seed}");
        }
    }
    assert!(
        offered > 10 && silent > 10,
        "both outcomes are covered: {offered} offered, {silent} not"
    );
}

fn last_seen_of(fx: &Fx, who: &str) -> Option<i64> {
    fx.db()
        .query_row(
            "SELECT last_seen_at FROM agents WHERE agent_id = ?1",
            [fx.agent_id(who)],
            |r| r.get(0),
        )
        .unwrap()
}

// Q10 T2
#[test]
fn offer_reads_only() {
    let fx = Fx::new();
    fx.store.queue_set_limit(&owner(), "ann", 2).unwrap();
    fx.store.queue_set_limit(&owner(), "bob", 1).unwrap();
    let lapsed = fx.ready("claimed by bob, then silence");
    fx.claim("bob");
    let held = fx.ready("claimed by ann in session s1");
    fx.start("ann", "s1");
    fx.claim("ann");
    let head = fx.ready("ready for ann");
    // Bob is past the grace period: a write by ann would release his claim.
    fx.advance(DAY + HOUR);
    let seen_before = last_seen_of(&fx, "ann");
    let events_before = fx.store.events(100_000).unwrap().len();
    let before = fx.snapshot();

    let read_only =
        Store::open_read_only(&fx.home(), std::time::Duration::from_millis(50)).unwrap();
    let offer = read_only.queue_offer("ann").unwrap();
    assert_eq!(offer.agent, "ann");
    assert!(offer.seated);
    assert_eq!(offer.limit, 2);
    assert_eq!(offer.held.len(), 1);
    assert_eq!(offer.held[0].label, held.label);
    assert_eq!(offer.held[0].session.as_deref(), Some("s1"));
    assert_eq!(
        offer.ready.as_deref(),
        Some(format!("{}:0", head.item_id).as_str()),
        "the mark is the item's id and the time it last came back to ready"
    );
    assert_eq!(offer.announce(), offer.ready.as_deref());

    assert_eq!(fx.snapshot(), before, "nothing in the queue changed");
    assert_eq!(fx.store.events(100_000).unwrap().len(), events_before);
    assert_eq!(last_seen_of(&fx, "ann"), seen_before, "no sign of life");
    assert_eq!(
        fx.item(&lapsed.label).state,
        QueueState::Claimed,
        "the lapsed claim was not released by the read"
    );

    // A name on the default limit is offered the same item by `next`; announce() is silent.
    let default_seat = read_only.queue_offer("cy").unwrap();
    assert!(!default_seat.seated);
    assert_eq!(default_seat.limit, DEFAULT_QUEUE_LIMIT);
    assert!(default_seat.ready.is_some());
    assert_eq!(default_seat.announce(), None);
    // A limit of 0 is no seat.
    fx.store.queue_set_limit(&owner(), "cy", 0).unwrap();
    let no_seat = fx.store.queue_offer("cy").unwrap();
    assert!(!no_seat.seated);
    assert_eq!((no_seat.limit, no_seat.ready), (0, None));
}

/// The ready mark changes exactly when the item comes back to ready: a note or an edit
/// leaves it as it was (spec 3.1).
#[test]
fn the_ready_mark_changes_only_on_a_return_to_ready() {
    let fx = Fx::new();
    fx.store.queue_set_limit(&owner(), "ann", 1).unwrap();
    let head = fx.ready("the head of the queue");
    let mark = |fx: &Fx| fx.store.queue_offer("ann").unwrap().ready.unwrap();
    let first = mark(&fx);
    fx.store.queue_note("bob", &head.label, "a note").unwrap();
    fx.store
        .queue_edit(
            &owner(),
            &head.label,
            EditItem {
                done_when: Some("the tests still pass".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(mark(&fx), first);

    fx.store.queue_set_limit(&owner(), "bob", 1).unwrap();
    fx.claim("bob");
    assert_eq!(fx.store.queue_offer("ann").unwrap().ready, None);
    fx.advance(HOUR);
    fx.store
        .queue_release(&agent("bob"), &head.label, ReleaseItem::default())
        .unwrap();
    let again = mark(&fx);
    assert!(again.starts_with(&format!("{}:", head.item_id)), "{again}");
    assert_ne!(again, first, "a return to ready is a new mark");
    assert_eq!(queue_stamp(&again).len(), 16);
    assert!(queue_stamp(&again).chars().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(queue_stamp(&again), queue_stamp(&first));
}

// Q10 T3
#[test]
fn one_selection() {
    let source = include_str!("../src/queue.rs");
    for needle in [
        "WHERE state = 'ready' AND node_id",
        "ORDER BY priority, admitted_at, seq",
    ] {
        assert_eq!(
            source.matches(needle).count(),
            1,
            "one copy of the ready-row selection: {needle:?}"
        );
    }
    assert_eq!(
        source.matches("first_ready(").count(),
        3,
        "the selection is defined once and called by `next` and by the offer"
    );
}
