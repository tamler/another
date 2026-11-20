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
        let store = Store::open_with_clock(&home, clock).unwrap();
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
        self.store.acquire_holds(AcquireHolds {
            agent: agent.into(),
            project: project.into(),
            paths: paths.iter().map(|p| p.to_string()).collect(),
            ttl_seconds: None,
            exclusive: true,
            reason: None,
            repo_wide: false,
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
        let secret: String = self
            .mallory
            .sql()
            .query_row("SELECT secret_key FROM nodes WHERE is_local = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        let key = crypto::decode_secret(&secret).unwrap();
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let payload = payload.to_string();
        let node_id = self.mallory.id();
        let event_id = format!("forged-{n}");
        let ts = T0 - 1000 + n as i64;
        let sig = crypto::sign(
            &key,
            &crypto::event_message(&event_id, &node_id, ts, "mal", kind, &payload),
        );
        let ev = WireEvent {
            seq: 1_000_000 + n as i64,
            event_id,
            node_id,
            ts,
            actor: "mal".into(),
            kind: kind.into(),
            payload,
            prev_id: None,
            sig,
        };
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

/// Rule B for a short pattern: one inside the limit that is not a glob globset compiles.
/// A `hold.updated` is held to the same rule.
#[test]
fn a_peers_short_pattern_that_does_not_compile_is_not_applied() {
    let fed = Fed::new();
    for (i, pattern) in ["src/{a", "src/[z-a]*", "src/*a}"].iter().enumerate() {
        let id = format!("short-{i}");
        let (ev, report) = fed.forge_hold(&id, pattern);
        assert_eq!(report.accepted, 1, "{pattern}: {report:?}");
        assert_eq!(fed.hub.event_rows(&ev.event_id), 1);
        assert_eq!(fed.hub.hold_rows(&id), 0, "{pattern} must not be applied");
        let listed = not_applied(&report);
        assert_eq!(listed.len(), 1, "{pattern}: {report:?}");
        let reason = listed[0]["reason"].as_str().unwrap();
        assert!(
            reason.ends_with(&format!(
                "its pattern {pattern:?} ({} bytes) is not a glob that compiles",
                pattern.len()
            )),
            "{reason}"
        );
    }
    let (_, report) = fed.forge(
        "hold.updated",
        json!({
            "hold_id": "updated", "agent_id": fed.mal_agent, "project_id": fed.mal_project,
            "pattern": two_mib(), "exclusive": true, "ttl_seconds": 1800,
        }),
    );
    assert_eq!(report.accepted, 1);
    assert_eq!(fed.hub.hold_rows("updated"), 0);
    assert_eq!(refusals(&fed.hub).len(), 4);
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
