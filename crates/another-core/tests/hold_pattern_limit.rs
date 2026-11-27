//! A hold pattern cannot panic, abort or block a command, whoever wrote it (Q30).
//!
//! The finding: a paired spoke signs a `hold.acquired` in a project named like the hub's,
//! with a glob of about 2 MiB (`src/` then `*a` repeated). The hub stored it, and its next
//! `hold acquire` in that project panicked inside globset. These tests cover the rules that
//! close it: every pattern compile is fallible, a pattern has one size limit for local and
//! peer input, a peer's unusable pattern is stored and relayed but never applied, and a
//! stored row that cannot be read fails closed in its own project only.
//!
//! New fields are read through JSON so that this file also builds against the source
//! before the fix, where each test fails by a panic or an assertion.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use another_core::test_support::forge_event;
use another_core::*;
use rusqlite::Connection;
use serde_json::{json, Value};

const T0: i64 = 1_800_000_000_000;
const LIMIT: usize = 4096;

/// The review's pattern: `src/` then `*a` repeated to about 2 MiB.
fn two_mib() -> String {
    format!("src/{}", "*a".repeat(1 << 20))
}

struct Node {
    store: Store,
    db: std::path::PathBuf,
    name: String,
}

impl Node {
    fn new(dir: &Path, name: &str) -> Node {
        let home = dir.join(name);
        let (clock, _) = Clock::manual(T0);
        let store = Store::open_with_clock(&home, clock)
            .unwrap()
            .unguarded_repair();
        store.set_node_name(name).unwrap();
        Node {
            store,
            db: home.join(DB_FILE),
            name: name.into(),
        }
    }

    fn sql(&self) -> Connection {
        Connection::open(&self.db).unwrap()
    }

    fn id(&self) -> String {
        self.store.node_id().to_string()
    }

    /// Register `agent` in a project directory called `project`; returns the directory.
    fn agent(&self, dir: &Path, agent: &str, project: &str) -> String {
        let path = dir.join(format!("{}-work", self.name)).join(project);
        std::fs::create_dir_all(&path).unwrap();
        let path = path.to_string_lossy().into_owned();
        self.store
            .register_agent(RegisterAgent {
                name: agent.into(),
                program: Program::Claude,
                project: Some(path.clone()),
                model: None,
                description: None,
            })
            .unwrap();
        path
    }

    fn acquire(&self, agent: &str, project: &str, paths: &[&str]) -> Result<HoldReport> {
        self.acquire_as(agent, project, paths, false)
    }

    /// [`Node::acquire`] with `--repo-wide`, for a glob that starts with a wildcard.
    fn acquire_wide(&self, agent: &str, project: &str, path: &str) -> HoldReport {
        self.acquire_as(agent, project, &[path], true).unwrap()
    }

    fn acquire_as(
        &self,
        agent: &str,
        project: &str,
        paths: &[&str],
        repo_wide: bool,
    ) -> Result<HoldReport> {
        self.store.acquire_holds(AcquireHolds {
            agent: agent.into(),
            project: project.into(),
            paths: paths.iter().map(|p| p.to_string()).collect(),
            ttl_seconds: None,
            exclusive: true,
            reason: None,
            repo_wide,
        })
    }

    fn hold_rows(&self, hold_id: &str) -> i64 {
        self.sql()
            .query_row(
                "SELECT count(*) FROM holds WHERE hold_id = ?1",
                [hold_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn event_rows(&self, event_id: &str) -> i64 {
        self.sql()
            .query_row(
                "SELECT count(*) FROM events WHERE event_id = ?1",
                [event_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// Rewrite a stored hold's pattern, as a row written by an older build would hold it.
    fn plant(&self, hold_id: &str, pattern: &str) {
        let n = self
            .sql()
            .execute(
                "UPDATE holds SET pattern = ?2 WHERE hold_id = ?1",
                (hold_id, pattern),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    /// `copies` more rows like the stored hold `like`, each with `pattern`: what the same
    /// number of that node's events would have stored, without signing each one.
    fn plant_copies(&self, like: &str, copies: usize, pattern: &str) {
        self.plant_each(like, (0..copies).map(|_| pattern.to_string()));
    }

    /// One more row like the stored hold `like` for each of `patterns`, in that order.
    fn plant_each(&self, like: &str, patterns: impl Iterator<Item = String>) {
        let mut db = self.sql();
        let tx = db.transaction().unwrap();
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO holds (hold_id, node_id, project_id, agent_id, pattern, exclusive,
                       reason, ttl_seconds, created_at, renewed_at, expires_at)
                     SELECT ?2, node_id, project_id, agent_id, ?3, exclusive, reason,
                       ttl_seconds, created_at, renewed_at, expires_at
                     FROM holds WHERE hold_id = ?1",
                )
                .unwrap();
            let first: i64 = tx
                .query_row("SELECT count(*) FROM holds", [], |r| r.get(0))
                .unwrap();
            for (i, pattern) in patterns.enumerate() {
                let n = insert
                    .execute((like, format!("{like}-copy-{}", first + i as i64), pattern))
                    .unwrap();
                assert_eq!(n, 1);
            }
        }
        tx.commit().unwrap();
    }
}

fn pair(hub: &Node, spoke: &Node) {
    let code = hub
        .store
        .create_invite("https://hub.example", &Perm::ALL, 3600)
        .unwrap();
    let (invite, req) = spoke.store.prepare_join(&code).unwrap();
    let resp = hub.store.accept_pairing(&req).unwrap();
    spoke.store.complete_join(&invite, &resp).unwrap();
}

/// Push everything `spoke` authored to the hub, as `another sync` does.
fn push(hub: &Node, spoke: &Node) -> IngestReport {
    let (events, _) = spoke.store.outbound_events(0, 10_000).unwrap();
    hub.store
        .ingest(&spoke.id(), &events, &[spoke.store.node_entry().unwrap()])
        .unwrap()
}

/// The hub is the node under test. Mallory is a paired spoke with a project of the same
/// name (`shop`); alpha is a second spoke the hub relays to.
struct Fed {
    _dir: tempfile::TempDir,
    hub: Node,
    alpha: Node,
    mallory: Node,
    /// The hub's project directories.
    shop: String,
    api: String,
    mal_agent: String,
    mal_project: String,
    seq: AtomicU64,
}

impl Fed {
    fn new() -> Fed {
        let dir = tempfile::tempdir().unwrap();
        let hub = Node::new(dir.path(), "hub");
        let alpha = Node::new(dir.path(), "alpha");
        let mallory = Node::new(dir.path(), "mallory");
        pair(&hub, &alpha);
        pair(&hub, &mallory);
        let shop = hub.agent(dir.path(), "alice", "shop");
        hub.agent(dir.path(), "bob", "shop");
        let api = hub.agent(dir.path(), "carol", "api");
        let mal_shop = mallory.agent(dir.path(), "mal", "shop");
        let report = push(&hub, &mallory);
        assert!(report.rejected.is_empty(), "{:?}", report.rejected);
        let mal_agent = mallory.store.agents().unwrap()[0].agent_id.clone();
        let mal_project = mallory.store.project(&mal_shop).unwrap().project_id;
        Fed {
            _dir: dir,
            hub,
            alpha,
            mallory,
            shop,
            api,
            mal_agent,
            mal_project,
            seq: AtomicU64::new(0),
        }
    }

    /// A correctly signed event from mallory, pushed to the hub. Returns the event too.
    fn forge(&self, kind: &str, payload: Value) -> (WireEvent, IngestReport) {
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let ev = forge_event(
            &self.mallory.sql(),
            WireEvent {
                seq: 1_000_000 + n as i64,
                event_id: format!("forged-{n}"),
                node_id: self.mallory.id(),
                ts: T0 - 1000 + n as i64,
                actor: "mal".into(),
                kind: kind.into(),
                payload: payload.to_string(),
                prev_id: None,
                sig: String::new(),
            },
        );
        let report = self
            .hub
            .store
            .ingest(
                &self.mallory.id(),
                std::slice::from_ref(&ev),
                &[self.mallory.store.node_entry().unwrap()],
            )
            .unwrap();
        (ev, report)
    }

    /// Mallory's signed `hold.acquired` in its project `shop`, with `pattern`.
    fn forge_hold(&self, hold_id: &str, pattern: &str) -> (WireEvent, IngestReport) {
        self.forge(
            "hold.acquired",
            json!({
                "hold_id": hold_id,
                "agent_id": self.mal_agent,
                "project_id": self.mal_project,
                "pattern": pattern,
                "exclusive": true,
                "reason": null,
                "ttl_seconds": 1800,
                "expires_at": T0 + 1_800_000,
            }),
        )
    }

    /// A hold mallory really took, synced to the hub, then rewritten on the hub to
    /// `pattern`: a foreign row as a build before this fix stored it.
    fn stored_foreign_hold(&self, pattern: &str) -> String {
        let (_, report) = self.forge_hold("old-foreign-hold", "src/old.rs");
        assert_eq!(report.accepted, 1, "{report:?}");
        assert_eq!(self.hub.hold_rows("old-foreign-hold"), 1);
        self.hub.plant("old-foreign-hold", pattern);
        "old-foreign-hold".into()
    }
}

fn not_applied(report: &IngestReport) -> Vec<Value> {
    serde_json::to_value(report).unwrap()["not_applied"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn note(conflict: &HoldConflict) -> String {
    serde_json::to_value(conflict).unwrap()["unreadable"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// The refusals on the hub's audit log.
fn refusals(node: &Node) -> Vec<AuditEntry> {
    node.store
        .audit_log(100)
        .unwrap()
        .into_iter()
        .filter(|e| e.operation == "federation hold pattern")
        .collect()
}

/// The finding itself, in the review's shape: a same-named project and a 2 MiB pattern.
/// Every command that reads holds still works on the hub.
#[test]
fn a_peers_two_mib_pattern_cannot_panic_hold_acquire() {
    let fed = Fed::new();
    let (_, report) = fed.forge_hold("big", &two_mib());
    assert!(report.rejected.is_empty(), "{:?}", report.rejected);
    let got = fed.hub.acquire("alice", &fed.shop, &["src/a.rs"]).unwrap();
    assert_eq!(got.granted.len(), 1, "{got:?}");
    assert!(got.conflicts.is_empty());
    fed.hub
        .store
        .check_holds("bob", &fed.shop, &["src/*.rs".to_string()], true)
        .unwrap();
    let guard = fed
        .hub
        .store
        .guard_edit("alice", &fed.shop, &["src/b.rs".to_string()], "editing")
        .unwrap();
    assert_eq!(guard.held.len(), 1, "{guard:?}");
    assert_eq!(fed.hub.store.holds(HoldFilter::default()).unwrap().len(), 2);
    fed.hub
        .store
        .dashboard_snapshot(DashboardView::LOCAL)
        .unwrap();
}

/// Rule B: the event is accepted, stored and relayed, so sync goes on; it is not applied
/// as a hold; and the refusal is on the audit log and in the ingest report, with the node,
/// the event id, a short prefix and the length, never the pattern whole.
#[test]
fn a_peers_over_limit_pattern_is_stored_and_relayed_but_never_applied() {
    let fed = Fed::new();
    let (ev, report) = fed.forge_hold("big", &two_mib());
    assert_eq!(report.accepted, 1, "{report:?}");
    assert!(report.rejected.is_empty(), "{:?}", report.rejected);
    assert_eq!(fed.hub.event_rows(&ev.event_id), 1, "the event is stored");
    assert_eq!(fed.hub.hold_rows("big"), 0, "the hold is not applied");

    let listed = not_applied(&report);
    assert_eq!(listed.len(), 1, "{report:?}");
    assert_eq!(listed[0]["event_id"], ev.event_id.as_str());
    let reason = listed[0]["reason"].as_str().unwrap();
    assert_eq!(
        reason,
        format!(
            "hold.acquired from node mallory ({}) was stored and relayed but not applied as a \
             hold: its pattern \"src/*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*…\" (2097156 bytes) is \
             longer than the limit of 4096 bytes",
            fed.mallory.id()
        )
    );

    let rows = refusals(&fed.hub);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0].agent.as_deref(),
        Some(format!("node:{}", fed.mallory.id()).as_str())
    );
    assert_eq!(rows[0].outcome, "refused");
    assert_eq!(
        rows[0].detail.as_deref(),
        Some(format!("event {}: {reason}", ev.event_id).as_str())
    );

    // Relayed: the second spoke is sent the event, stores it, and does not apply it either.
    let page = fed.hub.store.pull_for(&fed.alpha.id(), 0, 10_000).unwrap();
    assert!(page.events.iter().any(|e| e.event_id == ev.event_id));
    let relayed = fed
        .alpha
        .store
        .ingest(&fed.hub.id(), &page.events, &page.nodes)
        .unwrap();
    assert!(relayed.rejected.is_empty(), "{:?}", relayed.rejected);
    assert_eq!(fed.alpha.event_rows(&ev.event_id), 1);
    assert_eq!(fed.alpha.hold_rows("big"), 0);
    assert_eq!(not_applied(&relayed).len(), 1);
    assert_eq!(refusals(&fed.alpha).len(), 1);

    // Sync did not stall: the same node's next hold is applied.
    let (_, next) = fed.forge_hold("after", "src/after.rs");
    assert_eq!(next.accepted, 1);
    assert_eq!(fed.hub.hold_rows("after"), 1);
    // A second delivery of the event is a duplicate and writes no second audit row.
    let again = fed
        .hub
        .store
        .ingest(
            &fed.mallory.id(),
            std::slice::from_ref(&ev),
            &[fed.mallory.store.node_entry().unwrap()],
        )
        .unwrap();
    assert_eq!(again.duplicates, 1);
    assert_eq!(refusals(&fed.hub).len(), 1);
}

/// Nothing is compiled when a peer's event is stored (review R2 of Q30: that compile ran
/// inside the write transaction, 2 ms a glob). A pattern inside the limit is applied
/// whether or not it compiles, with no report. That it does not compile is found when it
/// is compared: it then counts as overlapping every path in its project, as a warning
/// that says so (rule C). Only the length decides at arrival, for `hold.updated` too.
#[test]
fn a_peers_short_pattern_that_does_not_compile_is_applied_and_warns_when_compared() {
    for (i, pattern) in ["src/{a", "src/[z-a]*", "src/*a}"].iter().enumerate() {
        let fed = Fed::new();
        let id = format!("short-{i}");
        let (ev, report) = fed.forge_hold(&id, pattern);
        assert_eq!(report.accepted, 1, "{pattern}: {report:?}");
        assert_eq!(fed.hub.event_rows(&ev.event_id), 1);
        assert_eq!(fed.hub.hold_rows(&id), 1, "{pattern} is applied");
        assert!(not_applied(&report).is_empty(), "{pattern}: {report:?}");
        assert!(refusals(&fed.hub).is_empty());
        let got = fed
            .hub
            .acquire("alice", &fed.shop, &["docs/unrelated.md"])
            .unwrap();
        assert_eq!(got.granted.len(), 1, "{got:?}");
        assert_eq!(got.warnings.len(), 1, "{pattern}: {got:?}");
        let text = note(&got.warnings[0]);
        assert!(
            text.starts_with(&format!("hold {id} of "))
                && text.contains(&format!(
                    "on node mallory has a pattern that could not be read ({pattern:?} ({} \
                     bytes)), so it counts as overlapping every path in project shop.",
                    pattern.len()
                )),
            "{text}"
        );
    }
    let fed = Fed::new();
    let (_, report) = fed.forge(
        "hold.updated",
        json!({
            "hold_id": "updated", "agent_id": fed.mal_agent, "project_id": fed.mal_project,
            "pattern": two_mib(), "exclusive": true, "ttl_seconds": 1800,
        }),
    );
    assert_eq!(report.accepted, 1);
    assert_eq!(fed.hub.hold_rows("updated"), 0);
    assert_eq!(refusals(&fed.hub).len(), 1);
    // A path with no glob syntax is compared as text and never compiled: it is applied,
    // even one that would not be a valid glob (an unescaped file name from an older build).
    let (_, report) = fed.forge_hold("plain", "src/odd}name.rs");
    assert_eq!(report.accepted, 1);
    assert!(not_applied(&report).is_empty(), "{report:?}");
    assert_eq!(fed.hub.hold_rows("plain"), 1);
    fed.hub.acquire("alice", &fed.shop, &["src/*.rs"]).unwrap();
}

/// The limit is the same number for a peer as for a local caller, to the byte.
#[test]
fn a_peers_pattern_at_the_limit_is_applied_and_one_byte_more_is_not() {
    let fed = Fed::new();
    let at = format!("src/{}", "*a".repeat((LIMIT - 4) / 2));
    assert_eq!(at.len(), LIMIT);
    let (_, report) = fed.forge_hold("at", &at);
    assert!(not_applied(&report).is_empty(), "{report:?}");
    assert_eq!(fed.hub.hold_rows("at"), 1);
    let (_, report) = fed.forge_hold("over", &format!("{at}b"));
    assert_eq!(not_applied(&report).len(), 1, "{report:?}");
    assert_eq!(fed.hub.hold_rows("over"), 0);
    // The applied one is an ordinary foreign hold: it warns where it overlaps (a name of
    // 2,046 letters `a`) and nowhere else.
    let name = format!("src/{}", "a".repeat((LIMIT - 4) / 2));
    let got = fed.hub.acquire("alice", &fed.shop, &[&name]).unwrap();
    assert_eq!(got.granted.len(), 1);
    assert_eq!(got.warnings.len(), 1, "{:?}", got.warnings.len());
    assert_eq!(note(&got.warnings[0]), "");
    // A long pattern in a warning is its start and its length, never 4 KB.
    assert_eq!(
        got.warnings[0].held.pattern,
        "\"src/*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*…\" (4096 bytes)"
    );
    let got = fed.hub.acquire("alice", &fed.shop, &["src/aa"]).unwrap();
    assert!(got.warnings.is_empty());
}

/// Rule C for a peer's row stored before patterns were checked on arrival: it counts as
/// overlapping, in the project it is in and nowhere else. A foreign hold never blocks, so
/// what the caller gets is the warning, naming the node and how the hold ends.
#[test]
fn a_stored_foreign_pattern_that_cannot_be_read_warns_in_its_project_only() {
    for pattern in [two_mib(), "src/{a".to_string()] {
        let fed = Fed::new();
        let id = fed.stored_foreign_hold(&pattern);
        let got = fed
            .hub
            .acquire("alice", &fed.shop, &["docs/unrelated.md"])
            .unwrap();
        assert_eq!(got.granted.len(), 1, "a foreign hold never blocks: {got:?}");
        assert_eq!(got.warnings.len(), 1, "{got:?}");
        let warning = &got.warnings[0];
        assert!(warning.held.pattern.len() < 80, "{}", warning.held.pattern);
        let text = note(warning);
        let brief = if pattern.len() > LIMIT {
            "\"src/*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*a*…\" (2097156 bytes)"
        } else {
            "\"src/{a\" (6 bytes)"
        };
        assert_eq!(
            text,
            format!(
                "hold {id} of {} on node mallory has a pattern that could not be read \
                 ({brief}), so it counts as overlapping every path in project shop. It ends at \
                 {} unless that node renews it; to clear it sooner, release hold {id} on node \
                 mallory and sync",
                warning.held.agent, warning.held.expires_at
            )
        );
        // Another project on this node is not touched by it.
        let other = fed.hub.acquire("carol", &fed.api, &["src/a.rs"]).unwrap();
        assert_eq!(other.granted.len(), 1);
        assert!(other.warnings.is_empty(), "{other:?}");
        // The dashboard compares local holds with foreign ones.
        let snap = fed
            .hub
            .store
            .dashboard_snapshot(DashboardView::LOCAL)
            .unwrap();
        assert_eq!(snap.counts.remote_warnings, 1);
    }
}

/// Rule C for a local row: the check fails closed for that row, so nothing overlapping is
/// granted in its project and the edit guard denies; other projects are untouched, every
/// command still runs, and the refusal says how to clear the hold, which then works.
#[test]
fn a_stored_local_pattern_that_cannot_be_read_blocks_its_project_only_and_can_be_cleared() {
    for pattern in [two_mib(), "src/{a".to_string()] {
        let fed = Fed::new();
        let bobs = fed.hub.acquire("bob", &fed.shop, &["docs/x.md"]).unwrap();
        let id = bobs.granted[0].hold_id.clone();
        let holder = bobs.granted[0].agent.clone();
        fed.hub.plant(&id, &pattern);

        let got = fed.hub.acquire("alice", &fed.shop, &["src/a.rs"]).unwrap();
        assert!(got.granted.is_empty(), "{got:?}");
        assert_eq!(got.conflicts.len(), 1, "{got:?}");
        let text = note(&got.conflicts[0]);
        assert!(
            text.starts_with(&format!(
                "hold {id} of {holder} on this node (hub) has a pattern that could not be read ("
            )),
            "{text}"
        );
        assert!(
            text.ends_with(&format!(
                "so it counts as overlapping every path in project shop. To clear it: another \
                 hold release --agent {holder} {id}"
            )),
            "{text}"
        );
        assert!(text.len() < 500, "{text}");
        assert!(got.conflicts[0].held.pattern.len() < 80);

        // The edit guard denies (a conflict), it does not fail: the hook fails open on errors.
        let guard = fed
            .hub
            .store
            .guard_edit("alice", &fed.shop, &["src/a.rs".to_string()], "editing")
            .unwrap();
        assert!(guard.held.is_empty(), "{guard:?}");
        assert_eq!(guard.conflicts.len(), 1, "{guard:?}");
        assert_eq!(note(&guard.conflicts[0]), text);

        // Only that project: another one is free, and listing and sweeping still work.
        let other = fed.hub.acquire("carol", &fed.api, &["src/a.rs"]).unwrap();
        assert_eq!(other.granted.len(), 1, "{other:?}");
        fed.hub.store.holds(HoldFilter::default()).unwrap();
        fed.hub.store.sweep().unwrap();
        fed.hub
            .store
            .dashboard_snapshot(DashboardView::LOCAL)
            .unwrap();

        // Asking the holder to release it works too, and the mail does not carry the
        // pattern whole (a message body takes 256 KiB at most).
        let ask = fed.hub.store.request_hold_release("alice", &id).unwrap();
        assert!(ask.body.len() < 600, "{} bytes", ask.body.len());
        assert!(ask.body.contains(&format!("({} bytes)", pattern.len())));

        // Cleared as the message says.
        let released = fed
            .hub
            .store
            .release_holds("bob", std::slice::from_ref(&id))
            .unwrap();
        assert_eq!(released, vec![id]);
        let got = fed.hub.acquire("alice", &fed.shop, &["src/a.rs"]).unwrap();
        assert_eq!(got.granted.len(), 1, "{got:?}");
    }
}

/// An agent's own hold that cannot be read covers nothing: its edit takes a narrow hold of
/// its own instead of resting on a pattern nobody can check.
#[test]
fn an_own_hold_that_cannot_be_read_covers_nothing() {
    let fed = Fed::new();
    let mine = fed.hub.acquire("alice", &fed.shop, &["docs/x.md"]).unwrap();
    fed.hub.plant(&mine.granted[0].hold_id, &two_mib());
    let guard = fed
        .hub
        .store
        .guard_edit("alice", &fed.shop, &["src/a.rs".to_string()], "editing")
        .unwrap();
    assert!(guard.covered.is_empty(), "{guard:?}");
    assert_eq!(guard.held.len(), 1, "{guard:?}");
    assert_eq!(guard.held[0].pattern, "src/a.rs");
}

/// The edit guard never reads another node's holds, so beside a peer's stored pattern it
/// cannot read it holds the file as usual. This held before the fix too: it is the answer
/// to what the hook did in the state the review found, where it printed nothing.
#[test]
fn the_edit_guard_holds_a_file_beside_a_peers_stored_pattern_it_cannot_read() {
    let fed = Fed::new();
    fed.stored_foreign_hold(&two_mib());
    let guard = fed
        .hub
        .store
        .guard_edit("alice", &fed.shop, &["src/a.rs".to_string()], "editing")
        .unwrap();
    assert!(guard.conflicts.is_empty(), "{guard:?}");
    assert_eq!(guard.held.len(), 1, "{guard:?}");
    assert_eq!(guard.held[0].pattern, "src/a.rs");
}

/// The dashboard compares each local hold with every foreign one in a project of the same
/// name. A local hold taken before the peer's pattern arrived made its snapshot panic.
#[test]
fn the_dashboard_survives_a_peers_stored_pattern_it_cannot_read() {
    let fed = Fed::new();
    let mine = fed.hub.acquire("alice", &fed.shop, &["src/a.rs"]).unwrap();
    assert_eq!(mine.granted.len(), 1);
    fed.stored_foreign_hold(&two_mib());
    let snap = fed
        .hub
        .store
        .dashboard_snapshot(DashboardView::LOCAL)
        .unwrap();
    assert_eq!(snap.counts.remote_warnings, 1);
}

/// Rule D: a local pattern has the same limit, refused with its size and nothing written.
#[test]
fn a_local_pattern_over_the_limit_is_refused_with_its_size() {
    let fed = Fed::new();
    let at = format!("src/{}", "*a".repeat((LIMIT - 4) / 2));
    let got = fed.hub.acquire("alice", &fed.shop, &[&at]).unwrap();
    assert_eq!(got.granted.len(), 1, "a pattern at the limit is held");
    let events = fed.hub.store.events(100_000).unwrap().len();
    for pattern in [
        format!("{at}b"),
        format!("src/{}", "*a".repeat(150_000)),
        two_mib(),
    ] {
        let err = fed
            .hub
            .acquire("bob", &fed.shop, &[&pattern])
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            format!("hold pattern is {} bytes; the limit is 4096", pattern.len())
        );
        let err = fed
            .hub
            .store
            .check_holds("bob", &fed.shop, &[pattern], true)
            .unwrap_err()
            .to_string();
        assert!(err.ends_with("the limit is 4096"), "{err}");
    }
    assert_eq!(fed.hub.store.events(100_000).unwrap().len(), events);
}

/// Rule E, and D99 kept: the edit guard's own holds are file names with their glob
/// characters escaped. They are compared as text and never compiled, so they are not held
/// to the limit and a long path cannot make an edit fail: the file is held, it blocks
/// exactly that file, and it is not treated as a pattern that cannot be read.
#[test]
fn the_edit_guard_still_holds_a_path_longer_than_the_limit() {
    let fed = Fed::new();
    // 1,600 brackets in short directory names: three bytes each once escaped.
    let path = format!("src/{}x.rs", "[[[[[[[[/".repeat(200));
    let guard = fed
        .hub
        .store
        .guard_edit("alice", &fed.shop, std::slice::from_ref(&path), "editing")
        .unwrap();
    assert_eq!(guard.held.len(), 1, "{guard:?}");
    assert_eq!(guard.held[0].pattern.len(), path.len() + 2 * 1600);
    assert!(guard.held[0].pattern.len() > LIMIT);
    // Covered by its own hold the next time.
    let again = fed
        .hub
        .store
        .guard_edit("alice", &fed.shop, std::slice::from_ref(&path), "editing")
        .unwrap();
    assert_eq!(again.covered, vec![path.clone()]);
    // Another agent is stopped on that file as on any held file, and on nothing else.
    let other = fed
        .hub
        .store
        .guard_edit("bob", &fed.shop, std::slice::from_ref(&path), "editing")
        .unwrap();
    assert_eq!(other.conflicts.len(), 1, "{other:?}");
    assert_eq!(note(&other.conflicts[0]), "");
    let free = fed
        .hub
        .store
        .guard_edit("bob", &fed.shop, &["src/b.rs".to_string()], "editing")
        .unwrap();
    assert_eq!(free.held.len(), 1, "{free:?}");
    let got = fed.hub.acquire("bob", &fed.shop, &["docs/*.md"]).unwrap();
    assert_eq!(got.granted.len(), 1, "{got:?}");
}

/// A pattern inside the limit cannot make one comparison slow either. These shapes make
/// the walk compile a long component at many of its steps. Before the fix the first one
/// took 273 seconds against `x` in a debug build; now each takes about a second, and the
/// bound here is wide so that a slow machine does not fail it.
#[test]
fn a_pattern_inside_the_limit_cannot_make_a_comparison_slow() {
    let long = "*a".repeat(1900);
    let shapes = [
        format!("{}{long}", "{a,b}".repeat(12)),
        format!("{}{long}", "**/".repeat(12)),
        format!("{}{long}", "{**,x}/".repeat(20)),
    ];
    let path = format!("{}b", "d/".repeat(40));
    for slow in &shapes {
        assert!(slow.len() <= LIMIT);
        for literal in ["x", path.as_str()] {
            let start = Instant::now();
            let _ = patterns_overlap(slow, literal);
            let took = start.elapsed();
            assert!(
                took < Duration::from_secs(60),
                "{} against {literal:?} took {took:?}",
                &slow[..24]
            );
        }
    }
}

/// The review's cheap hostile shape: 86 bytes, fourteen alternations and four classes. It
/// compiles, so it is applied, and one comparison with it used to take 0.2 s in a release
/// build.
fn cheap_hostile() -> String {
    format!("{}[!a][!b][!c][!d]", "{a,b}".repeat(14))
}

fn not_compared(report: &HoldReport) -> Value {
    serde_json::to_value(report).unwrap()["not_compared"].clone()
}

/// A wide bound for one command, in a debug build on a loaded CI machine. The commands
/// below take well under a second here; before this bound existed the first of them took
/// 292 seconds in a release build with 1,000 such holds and did not finish in 600 with
/// 10,000.
const COMMAND_BOUND: Duration = Duration::from_secs(20);

fn timed<T>(what: &str, f: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let out = f();
    let took = start.elapsed();
    assert!(took < COMMAND_BOUND, "{what} took {took:?}");
    out
}

/// `hold acquire` with a hard limit: it runs on a thread of its own and the test fails at
/// the bound, where waiting for the answer would hang the run for as long as a regression
/// takes (on the head before the budget, more than fifteen minutes).
fn acquire_within_the_bound(node: &Node, agent: &str, project: &str, path: &str) -> HoldReport {
    let (answer, waiting) = std::sync::mpsc::channel();
    let store = node.store.clone();
    let input = AcquireHolds {
        agent: agent.into(),
        project: project.into(),
        paths: vec![path.into()],
        ttl_seconds: None,
        exclusive: true,
        reason: None,
        repo_wide: false,
    };
    std::thread::spawn(move || {
        let _ = answer.send(store.acquire_holds(input));
    });
    waiting
        .recv_timeout(COMMAND_BOUND)
        .unwrap_or_else(|_| panic!("hold acquire did not answer in {COMMAND_BOUND:?}"))
        .unwrap()
}

/// Review R1 of Q30, P1-1: a peer's many cheap holds. Each one compiles and is applied.
/// Comparing with them is best effort within one budget for the whole command, so `hold
/// acquire` answers at once, lists none of them one by one and says in one count how many
/// holds of which node were not compared. The check, the edit guard and the dashboard
/// answer at once too, and the guard still stops an edit of a file another agent holds.
#[test]
fn a_peers_many_cheap_holds_cost_one_count_and_no_time() {
    for holds in [1_000usize, 10_000] {
        let fed = Fed::new();
        let held = fed.hub.acquire("bob", &fed.shop, &["src/held.rs"]).unwrap();
        assert_eq!(held.granted.len(), 1);
        let (_, report) = fed.forge_hold("cheap", &cheap_hostile());
        assert!(not_applied(&report).is_empty(), "it compiles: {report:?}");
        assert_eq!(fed.hub.hold_rows("cheap"), 1);
        fed.hub.plant_copies("cheap", holds - 1, &cheap_hostile());

        let got = acquire_within_the_bound(&fed.hub, "alice", &fed.shop, "src/a.rs");
        assert_eq!(got.granted.len(), 1, "a foreign hold never blocks");
        assert!(got.warnings.is_empty(), "{} warnings", got.warnings.len());
        assert_eq!(
            not_compared(&got),
            json!([{ "node": "mallory", "holds": holds }]),
            "one count for all of them"
        );
        assert!(serde_json::to_string(&got).unwrap().len() < 1_000);

        timed("hold check", || {
            fed.hub
                .store
                .check_holds("alice", &fed.shop, &["src/*.rs".to_string()], true)
                .unwrap()
        });
        let guard = timed("the edit guard", || {
            fed.hub
                .store
                .guard_edit("alice", &fed.shop, &["src/b.rs".to_string()], "editing")
                .unwrap()
        });
        assert_eq!(guard.held.len(), 1, "{guard:?}");
        let stopped = timed("the edit guard on a held file", || {
            fed.hub
                .store
                .guard_edit("alice", &fed.shop, &["src/held.rs".to_string()], "editing")
                .unwrap()
        });
        assert_eq!(stopped.conflicts.len(), 1, "{stopped:?}");
        let snap = timed("the dashboard snapshot", || {
            fed.hub
                .store
                .dashboard_snapshot(DashboardView::LOCAL)
                .unwrap()
        });
        let counts = serde_json::to_value(&snap.counts).unwrap();
        assert_eq!(counts["remote_not_compared"], holds);
        assert_eq!(counts["remote_warnings"], 0);
        // Another project is not touched by any of it.
        let other = fed.hub.acquire("carol", &fed.api, &["src/a.rs"]).unwrap();
        assert_eq!(not_compared(&other), Value::Null);
    }
}

/// The budget is spent on what a peer sent, not on honest holds: a hold on a short plain
/// path costs 2 of the 40,000 (one to read it, one to compare it), so thousands of them
/// are compared and the one that overlaps is found. Past the budget the rest are counted,
/// never silently dropped: of 30,000 such holds 19,999 are compared.
#[test]
fn plain_holds_of_a_peer_are_compared_by_the_thousand() {
    let fed = Fed::new();
    let (_, report) = fed.forge_hold("plain", "src/other.rs");
    assert_eq!(report.accepted, 1);
    fed.hub.plant_copies("plain", 2_998, "src/other.rs");
    fed.forge_hold("mine", "src/a.rs");
    let got = timed("hold acquire", || {
        fed.hub.acquire("alice", &fed.shop, &["src/a.rs"]).unwrap()
    });
    assert_eq!(got.warnings.len(), 1, "{:?}", got.warnings.len());
    assert_eq!(got.warnings[0].held.hold_id, "mine");
    assert_eq!(not_compared(&got), Value::Null, "all 3,000 were compared");

    fed.hub.plant_copies("mine", 27_000, "src/unrelated.rs");
    let got = timed("hold acquire", || {
        fed.hub.acquire("alice", &fed.shop, &["src/b.rs"]).unwrap()
    });
    assert_eq!(
        not_compared(&got),
        json!([{ "node": "mallory", "holds": 10_001 }]),
        "30,000 holds, 19,999 compared"
    );
}

/// An honest peer's edit holds: one file each, on paths like a real monorepo's.
fn honest_path(i: usize) -> String {
    format!("apps/web/src/features/f{i}/components/table/Row{i}.tsx")
}

/// How many of the peer's holds in `shop` a request left uncompared.
fn left_uncompared(got: &HoldReport) -> u64 {
    not_compared(got).as_array().map_or(0, |nodes| {
        nodes.iter().map(|n| n["holds"].as_u64().unwrap()).sum()
    })
}

/// Review R2 of Q30, P2-B: with an honest peer and no hostile input, a glob request left
/// 142 of 300 plain holds uncompared, and with 2,000 another left 825 and missed a real
/// overlap. A requested glob is now matched directly against a hold on a plain path, so
/// every one is compared and every overlap is found.
#[test]
fn a_glob_request_is_compared_with_every_plain_hold_of_an_honest_peer() {
    for holds in [300usize, 2_000] {
        let fed = Fed::new();
        fed.forge_hold("first", &honest_path(0));
        fed.hub.plant_each("first", (1..holds).map(honest_path));
        // Taken last, and the only one under apps/api.
        fed.forge_hold("server", "apps/api/src/server.ts");
        for (request, overlaps) in [
            ("apps/api/src/*.ts", 1),
            ("apps/api/**", 1),
            ("apps/api/src/server.ts", 1),
            ("apps/api/src/other.ts", 0),
            // Every one of the peer's files is a .ts or .tsx file under apps/*/src.
            ("{apps,packages}/*/src/**/*.{ts,tsx}", holds + 1),
            // The walk's reading, kept: a path below `apps` may be a directory that holds
            // what the glob names, so it counts as overlapping.
            ("apps/**/{api,server}/**/*.ts", holds + 1),
            ("packages/**/*.ts", 0),
        ] {
            let got = fed.hub.acquire_wide("alice", &fed.shop, request);
            let json = serde_json::to_value(&got).unwrap();
            let found = got.warnings.len() as u64 + json["warnings_more"].as_u64().unwrap_or(0);
            assert_eq!(left_uncompared(&got), 0, "{request} with {holds} holds");
            assert_eq!(found, overlaps as u64, "{request} with {holds} holds");
            if overlaps == 1 {
                assert_eq!(got.warnings[0].held.hold_id, "server", "{request}");
            }
        }
    }
}

/// What one command compares of a peer's holds, as the docs state it: these are the
/// figures in docs/HOLDS.md. Holds on plain paths are compared as they are read and cost
/// about the same whatever is asked for. Glob holds are set aside and compared after, and
/// each costs a compile and, against a plain request, a walk: of 10,000 of them 541 are
/// compared for a path and 1,769 for a glob.
#[test]
fn what_one_command_compares_of_a_peers_holds() {
    let compared = |fed: &Fed, total: u64, request: &str| {
        total - left_uncompared(&fed.hub.acquire_wide("alice", &fed.shop, request))
    };
    // 30,000 holds on plain paths of about 50 bytes.
    let plain = Fed::new();
    plain.forge_hold("first", &honest_path(0));
    plain.hub.plant_each("first", (1..30_000).map(honest_path));
    assert_eq!(
        [
            compared(&plain, 30_000, "apps/api/src/server.ts"),
            compared(&plain, 30_000, "apps/api/src/*.ts"),
            compared(&plain, 30_000, "{apps,packages}/*/src/**/*.{ts,tsx}"),
        ],
        [19_999, 19_995, 19_991]
    );
    // 10,000 glob holds of about 30 bytes, the kind a person asks for.
    let globs = Fed::new();
    globs.forge_hold("first", "packages/pkg0/src/**/*.test.ts");
    globs.hub.plant_each(
        "first",
        (1..10_000).map(|i| format!("packages/pkg{i}/src/**/*.test.ts")),
    );
    assert_eq!(
        [
            compared(&globs, 10_000, "packages/pkg7/src/a.test.ts"),
            compared(&globs, 10_000, "packages/*/src/**/*.ts"),
        ],
        [541, 1_769]
    );
    // A plain hold stored after all those globs is still compared, and found.
    globs.forge_hold("plain", "apps/api/src/server.ts");
    let got = globs
        .hub
        .acquire("alice", &globs.shop, &["apps/api/src/*.ts"])
        .unwrap();
    assert_eq!(got.warnings.len(), 1);
    assert_eq!(got.warnings[0].held.hold_id, "plain");
}

/// The warnings themselves are bounded: ten overlapping holds are listed and the rest are
/// one number, where 1,000 overlapping holds used to be 465 KB of JSON.
#[test]
fn overlapping_holds_of_a_peer_are_listed_up_to_ten_and_then_counted() {
    let fed = Fed::new();
    fed.forge_hold("same", "src/a.rs");
    fed.hub.plant_copies("same", 999, "src/a.rs");
    let got = fed.hub.acquire("alice", &fed.shop, &["src/a.rs"]).unwrap();
    assert_eq!(got.granted.len(), 1);
    assert_eq!(got.warnings.len(), 10);
    let json = serde_json::to_value(&got).unwrap();
    assert_eq!(json["warnings_more"], 990);
    assert_eq!(json["not_compared"], Value::Null);
    let size = json.to_string().len();
    assert!(size < 6_000, "{size} bytes");
    let snap = fed
        .hub
        .store
        .dashboard_snapshot(DashboardView::LOCAL)
        .unwrap();
    assert_eq!(snap.counts.remote_warnings, 1_000);
    let listed: usize = snap.holds.iter().map(|h| h.remote_warnings.len()).sum();
    assert_eq!(listed, 10);
}
