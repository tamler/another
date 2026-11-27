//! `another needs`: the one list of what needs the owner (scope id M7, with C2), through
//! the real binary on temp homes. Test names follow the Q11 spec's test section.
//!
//! The binary reads the system clock, so a test makes a condition old by writing its
//! times into the home with SQL. Session rows are made by the hook's own store call and
//! then given the state a daemon sweep would have stored.

#[path = "common/transports.rs"]
mod transports;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use another_core::now_ms;
use another_core::{
    AdmitItem, Program, ProposeItem, QueueCaller, QueueItem, QueueKind, RegisterAgent, ReleaseItem,
    SendMessage, Store, TransportWake, WakeKind, WakeOutcome, DB_FILE,
};
use serde_json::{json, Value};

const MIN: i64 = 60_000;
const CANNOT_READ: &str = "needs: cannot read, run another doctor\n";
const KINDS: [&str; 11] = [
    "checks_stopped",
    "transport_config",
    "skipped_event",
    "decide_now",
    "blocked",
    "stuck",
    "unreachable",
    "transport_silent",
    "quiet_claim",
    "no_taker",
    "proposal",
];

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn another(home: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_another"));
    cmd.args(args)
        .env("ANOTHER_HOME", home)
        .env_remove("ANOTHER_AGENT")
        .env_remove("ANOTHER_ADMIT_KEY")
        .env_remove("ANOTHER_LOG")
        .env_remove("HERDR_BIN_PATH")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("run another")
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap_or_else(|| panic!("not a list: {v}"))
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

fn rows_of(v: &Value, kind: &str) -> Vec<Value> {
    v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["kind"] == kind)
        .cloned()
        .collect()
}

/// The one row of that kind.
fn one(v: &Value, kind: &str) -> Value {
    let mut rows = rows_of(v, kind);
    assert_eq!(rows.len(), 1, "one {kind} row in {v:#}");
    rows.remove(0)
}

struct Fx {
    _dir: tempfile::TempDir,
    home: PathBuf,
    store: Store,
}

impl Fx {
    fn new() -> Fx {
        let fx = Fx::empty();
        for name in ["ann", "bob", "cy", "arch", "dee", "eve"] {
            fx.register(name);
        }
        fx
    }

    /// A home with no agents.
    fn empty() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let store = Store::open(&home).unwrap();
        Fx {
            _dir: dir,
            home,
            store,
        }
    }

    fn register(&self, name: &str) {
        self.store
            .register_agent(RegisterAgent {
                name: name.into(),
                program: Program::Claude,
                project: None,
                model: None,
                description: None,
            })
            .unwrap();
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.home.join(DB_FILE)).unwrap()
    }

    fn exec(&self, sql: &str) {
        self.db()
            .execute_batch(sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    fn id(&self, agent: &str) -> String {
        self.store.agent(agent).unwrap().agent_id
    }

    /// Write this node's own marker for a skipped registration of agent `wordy` with the
    /// event id `event_id`, signed with the node's key, as a sync that stepped past the
    /// event writes it (Q26b).
    fn skip(&self, event_id: &str) {
        use another_core::crypto;
        let db = self.db();
        let secret: String = db
            .query_row("SELECT secret_key FROM nodes WHERE is_local = 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        let key = crypto::decode_secret(&secret).unwrap();
        let node = self.store.node_id().to_string();
        let payload = json!({
            "event_id": event_id,
            "event_node": node,
            "event_kind": "agent.registered",
            "event_actor": "wordy",
            "bytes": 9_437_500,
            "direction": "push",
            "about": "agent wordy",
            "route": "directory",
            "route_nodes": [],
        })
        .to_string();
        let id = format!("01MARKER{event_id}");
        let ts = now_ms() - 5 * MIN;
        let sig = crypto::sign(
            &key,
            &crypto::event_message(&id, &node, ts, "system", "sync.skipped", &payload),
        );
        db.execute(
            "INSERT INTO events (event_id, node_id, ts, actor, kind, payload, prev_id, sig)
             VALUES (?1, ?2, ?3, 'system', 'sync.skipped', ?4, NULL, ?5)",
            rusqlite::params![id, node, ts, payload, sig],
        )
        .unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        another(&self.home, args, &[])
    }

    /// `another needs` with those arguments: stdout, and exit 0.
    fn needs(&self, args: &[&str]) -> String {
        let mut all = vec!["needs"];
        all.extend_from_slice(args);
        let out = self.run(&all);
        assert_eq!(out.status.code(), Some(0), "{all:?}: {}", text(&out.stderr));
        text(&out.stdout)
    }

    fn text(&self) -> String {
        self.needs(&[])
    }

    fn json(&self) -> Value {
        self.json_with(&[])
    }

    fn json_with(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let out = self.needs(&all);
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("no JSON ({e}): {out:?}"))
    }

    fn brief(&self) -> String {
        self.needs(&["--brief"])
    }

    fn propose(&self, who: &str, title: &str, kind: QueueKind) -> QueueItem {
        self.store
            .queue_propose(
                &QueueCaller::agent(who),
                ProposeItem {
                    title: title.into(),
                    scope: Some("M7".into()),
                    done_when: Some("the tests pass".into()),
                    kind,
                    ..Default::default()
                },
            )
            .unwrap()
    }

    fn ready(&self, title: &str) -> QueueItem {
        self.ready_as(title, QueueKind::Work, AdmitItem::default())
    }

    fn ready_as(&self, title: &str, kind: QueueKind, admit: AdmitItem) -> QueueItem {
        let item = self.propose("ann", title, kind);
        self.store
            .queue_admit(&QueueCaller::Owner, &item.label, admit)
            .unwrap()
    }

    fn claim(&self, who: &str) -> QueueItem {
        self.store
            .queue_next(who)
            .unwrap()
            .claimed
            .unwrap_or_else(|| panic!("{who} was handed nothing"))
    }

    /// Bind the claim on `label` to session `sid` of `agent`, with that session's row.
    fn bind(&self, label: &str, agent: &str, sid: &str) {
        let now = now_ms();
        let seq = label.trim_start_matches('Q');
        self.exec(&format!(
            "INSERT OR REPLACE INTO queue_sessions (agent_id, session_id, started_at, seen_at)
               VALUES ('{id}', '{sid}', {now}, {now});
             UPDATE queue_items SET claim_session = '{sid}' WHERE seq = {seq};",
            id = self.id(agent)
        ));
    }

    /// Leave the claim on `label` bound to no session, as a claim made while two sessions
    /// of the name were fresh is.
    fn unbind(&self, label: &str) {
        self.exec(&format!(
            "UPDATE queue_items SET claim_session = NULL WHERE seq = {}",
            label.trim_start_matches('Q')
        ));
    }

    /// Nothing heard from `agent`, and nothing claimed by it, for `mins` minutes.
    fn quiet(&self, agent: &str, mins: i64) {
        let at = now_ms() - mins * MIN;
        self.exec(&format!(
            "UPDATE agents SET last_seen_at = {at} WHERE agent_id = '{id}';
             UPDATE queue_items SET claimed_at = {at} WHERE claimer_id = '{id}';",
            id = self.id(agent)
        ));
    }

    /// What a hook of that session writes when it runs inside a transport.
    fn session(&self, agent: &str, sid: &str, transport: &str, target: &str) {
        self.store
            .stamp_transport_session(agent, sid, transport, target, "claude", "/work/app")
            .unwrap();
    }

    fn set_session(&self, agent: &str, sid: &str, assignments: &str) {
        self.exec(&format!(
            "UPDATE transport_sessions SET {assignments}
             WHERE agent_id = '{}' AND session_id = '{sid}'",
            self.id(agent)
        ));
    }

    /// A Herdr session as the daemon's sweep leaves it once `state` has lasted `mins`
    /// minutes and put the row in attention for `reason`: checked just now, seen live.
    fn attention(
        &self,
        agent: &str,
        sid: &str,
        target: &str,
        state: &str,
        reason: &str,
        mins: i64,
    ) {
        self.session(agent, sid, "herdr", target);
        let now = now_ms();
        let since = now - mins * MIN;
        let matched = i32::from(state != "gone" && state != "no_answer");
        self.set_session(
            agent,
            sid,
            &format!(
                "state = '{state}', state_since = {since}, observed_at = {now},
                 matched = {matched}, seen_live_at = {since}, attention = '{reason}',
                 attention_since = {since}"
            ),
        );
    }

    fn mail(&self, from: &str, to: &str, subject: &str, body: &str) {
        self.store
            .send_message(SendMessage {
                from: from.into(),
                to: vec![to.into()],
                subject: subject.into(),
                body: body.into(),
                ..Default::default()
            })
            .unwrap();
    }

    /// Make `arch` a delegate; return it as a caller with its key.
    fn delegate(&self) -> QueueCaller {
        let made = self
            .store
            .queue_delegate_add(&QueueCaller::Owner, "arch")
            .unwrap();
        QueueCaller::Agent {
            agent: "arch".into(),
            admit_key: Some(made.key),
        }
    }

    fn failed_notice(&self, item: &QueueItem, to: &str) {
        let payload = json!({ "item_id": item.item_id, "seq": item.seq, "to": self.id(to),
            "reason": "owner", "error": "x" });
        self.db()
            .execute(
                "INSERT INTO events (event_id, node_id, ts, actor, kind, payload)
                 VALUES (upper(hex(randomblob(13))),
                         (SELECT node_id FROM nodes WHERE is_local = 1), ?1, 'seed',
                         'queue.notify_failed', ?2)",
                rusqlite::params![now_ms(), payload.to_string()],
            )
            .unwrap();
    }

    /// One counted row of every kind and all three closing lines. Returns the labels of
    /// (the quiet claim, the fast-lane proposal, the work proposal, the reserved item).
    fn everything(&self) -> [String; 4] {
        let arch = self.delegate();
        let claimed = self.ready("Hold fixes");
        assert_eq!(self.claim("eve").label, claimed.label);
        self.quiet("eve", 95);
        let urgent = self.propose("bob", "Token printed in debug output", QueueKind::Security);
        let proposal = self.propose("ann", "Flaky test in net.rs", QueueKind::Work);
        let own = self
            .store
            .queue_propose(
                &arch,
                ProposeItem {
                    title: "Tidy the docs".into(),
                    scope: Some("M7".into()),
                    done_when: Some("merged".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        self.store
            .queue_admit(&arch, &own.label, AdmitItem::default())
            .unwrap();
        // The self-admitted item is taken, so the reserved one is the only ready item.
        assert_eq!(self.claim("ann").label, own.label);
        let reserved = self.ready_as(
            "Hub backup restore",
            QueueKind::Work,
            AdmitItem {
                for_agent: Some("dee".into()),
                ..Default::default()
            },
        );
        self.failed_notice(&claimed, "eve");

        self.attention("ann", "7f3a1111", "w1:p2", "blocked", "blocked", 12);
        self.attention("bob", "5c0e2222", "w1:p3", "idle", "stuck", 25);
        let now = now_ms();
        self.set_session(
            "bob",
            "5c0e2222",
            &format!(
                "wake_stop = 'not_started', wake_stop_at = {now}, last_wake_at = {now},
                 last_wake_outcome = 'not_started', hook_seen_at = {}",
                now - 2 * MIN
            ),
        );
        self.attention("cy", "91c03333", "w2:p1", "unknown", "unknown", 30);
        self.attention("arch", "aa004444", "w2:p2", "no_answer", "no_answer", 15);
        self.attention("dee", "bb005555", "w3:p1", "gone", "gone", 120);
        // No check for six minutes while hooks keep running, and a config that is refused.
        self.exec(&format!(
            "UPDATE transport_sessions SET observed_at = {};
             UPDATE agents SET last_seen_at = {} WHERE agent_id = '{}';",
            now - 6 * MIN,
            now - 120 * MIN,
            self.id("dee")
        ));
        transports::write(&self.home, "{");
        [claimed.label, urgent.label, proposal.label, reserved.label]
    }
}

#[test]
fn nothing_waiting_is_one_short_line() {
    let fx = Fx::new();
    let out = fx.run(&["needs"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(text(&out.stdout), "nothing needs the owner\n");
    assert_eq!(text(&out.stderr), "");

    let v = fx.json();
    assert_eq!(
        v.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["now", "node", "total", "limit", "counts", "closing", "rows"]
    );
    assert_eq!(v["total"], 0);
    assert_eq!(v["limit"], 10);
    assert_eq!(v["rows"], json!([]));
    assert_eq!(
        v["counts"].as_object().unwrap().keys().collect::<Vec<_>>(),
        KINDS
    );
    assert!(v["counts"].as_object().unwrap().values().all(|n| n == 0));
    assert_eq!(
        v["closing"],
        json!({ "gone_idle": 0, "failed_notices": [],
                "self_admitted": { "recent": [], "total": 0 } })
    );
}

#[test]
fn pending_decisions_appear_in_the_one_list() {
    let fx = Fx::new();
    let arch = fx.delegate();
    let urgent = fx.propose("bob", "A leaked token", QueueKind::Security);
    let work = fx.propose("ann", "A flaky test", QueueKind::Work);
    let (u, w) = (urgent.label.as_str(), work.label.as_str());

    let v = fx.json();
    assert_eq!(v["total"], 2);
    let row = one(&v, "decide_now");
    assert_eq!(row["key"], format!("decide_now:{u}"));
    assert_eq!(row["who"], "owner");
    assert_eq!(row["item"], u);
    assert_eq!(row["item_kind"], "security");
    assert_eq!(row["scope"], "M7");
    assert_eq!(row["title"], "A leaked token");
    assert_eq!(row["agent"], "bob");
    assert_eq!(row["session"], Value::Null);
    assert_eq!(
        strs(&row["next"]),
        [
            format!("another queue show {u}"),
            format!("another queue admit {u} --owner"),
            format!("another queue reject {u} --reason \"<why>\" --owner"),
        ]
    );
    assert_eq!(row["next_delegate"], json!([]));

    let row = one(&v, "proposal");
    assert_eq!(row["key"], format!("proposal:{w}"));
    assert_eq!(row["who"], "admitter");
    assert_eq!(row["item_kind"], "work");
    assert_eq!(
        strs(&row["next"]),
        [
            format!("another queue show {w}"),
            format!("another queue admit {w} --owner"),
            format!("another queue reject {w} --reason \"<why>\" --owner"),
        ]
    );
    assert_eq!(
        strs(&row["next_delegate"]),
        [
            format!("another queue show {w}"),
            format!("another queue admit {w} --agent <delegate>"),
            format!("another queue reject {w} --reason \"<why>\" --agent <delegate>"),
        ]
    );

    let human = fx.text();
    assert!(human.starts_with("2 need the owner ("), "{human}");
    assert!(human.contains("\nDecide now (1)\n"), "{human}");
    assert!(
        human.contains(&format!("  {u} security  A leaked token  proposed by bob ")),
        "{human}"
    );
    assert!(human.contains("\nProposals (1)\n"), "{human}");
    assert!(
        human.contains(&format!(
            "    next: another queue show {w}\n          another queue admit {w} --owner\n"
        )),
        "{human}"
    );

    // A decision removes its row: the delegate admits the work item, the owner rejects
    // the fast-lane one.
    fx.store
        .queue_admit(&arch, w, AdmitItem::default())
        .unwrap();
    let v = fx.json();
    assert_eq!(v["total"], 1);
    assert!(rows_of(&v, "proposal").is_empty());
    fx.store
        .queue_reject(&QueueCaller::Owner, u, "not a leak")
        .unwrap();
    // The admitted item is ready and somebody can take it, so nothing is left.
    assert_eq!(fx.text(), "nothing needs the owner\n");
}

#[test]
fn sections_are_in_the_fixed_order() {
    let fx = Fx::new();
    fx.everything();
    fx.skip("01SK1PPED00000000000000000");
    let v = fx.json();
    assert_eq!(v["total"], 11, "{v:#}");
    let kinds: Vec<&str> = v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, KINDS);
    assert!(v["counts"].as_object().unwrap().values().all(|n| n == 1));
    assert_eq!(v["closing"]["gone_idle"], 1);
    assert_eq!(v["closing"]["failed_notices"].as_array().unwrap().len(), 1);
    assert_eq!(v["closing"]["self_admitted"]["total"], 1);

    let human = fx.text();
    let at = |needle: &str| {
        human
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} is not in:\n{human}"))
    };
    let order = [
        at("11 need the owner ("),
        at("\nAnother (3)\n"),
        at("  checks stopped  "),
        at("  transports.json refused  "),
        at("  skipped event  01SK1PPED00000000000000000 (agent.registered, 9437500 bytes) in this node's log "),
        at("\nDecide now (1)\n"),
        at("\nAgents (4)\n"),
        at("  blocked      ann 7f3a..  herdr w1:p2  12m"),
        at("  stuck        bob 5c0e..  herdr w1:p3  "),
        at("  unreachable  cy 91c0..  herdr w2:p1  unknown 30m"),
        at("  transport silent  herdr has not answered for 15m, 1 session"),
        at("\nClaims (2)\n"),
        at("claimed by eve, quiet for 95 min, not bound to a session, grace ends "),
        at("ready, reserved for dee, which cannot take it now"),
        at("\nProposals (1)\n"),
        at("\n1 session gone with nothing waiting: another wake prune --dry-run\n"),
        at("\n1 notice could not be sent in the last 24 hours (Q1 to eve): another queue show Q1\n"),
        at("\n1 self-admitted in the last 7 days (Q4), 1 in all: another queue list --self-admitted\n"),
    ];
    let mut sorted = order;
    sorted.sort_unstable();
    assert_eq!(order, sorted, "{human}");
    assert!(
        human.contains("last wake: delivered, did not start"),
        "{human}"
    );
    // Every row has its own next block.
    assert_eq!(human.matches("    next: ").count(), 11, "{human}");
}

/// The steps `another needs` prints under a skipped registration of agent `wordy`, then the
/// acknowledgement. They are the steps of SPEC Q26b section 4.10 in the wording this list
/// keeps: a bare entry is a complete command, and every other entry starts with a word and
/// has its commands in backticks.
const WORDY_STEPS: [&str; 7] = [
    "register the agent again: `another agent register wordy --program <program> --model <model> --description \"<4096 bytes or fewer>\"` (add `--project <path>` if it has one; a model left out is cleared)",
    "each hold it has (`another holds --agent wordy` lists them): `another hold acquire --agent wordy -p <project> <the same paths>`, with --shared, --repo-wide, --reason and --ttl as the hold has them (the same hold is sent again; without -p from another directory it is a different hold, and without --shared a shared hold becomes exclusive or is refused)",
    "each resource of this node it holds a lease on: `another release <resource id or kind:name> --agent wordy`, then at once `another borrow <the same> --agent wordy` (a borrow without the release only renews, and a renewal brings nothing to a node that never had the lease. Between the two commands the resource has no lease unless another agent holds one. Another agent on this node that borrows it in that moment becomes its owner, and if the resource is exclusive this agent's borrow is then refused. If another agent already holds a lease, that agent becomes the owner at the release and this agent comes back as a borrower)",
    "each resource of another node it borrowed: `another release <resource id> --agent wordy`, then `another borrow <resource id> --agent wordy` (the id is in brackets in `another resources`; kind:name names only this node's resources. If the borrow is refused, the release has already happened: it is refused when the hub does not grant borrowing, when the resource has no live owner, or when it cannot be reached from this machine)",
    "mail it sent to other nodes before step 1 is not delivered there and no command delivers it: send it again",
    "spokes receive it at their next sync",
    "another sync ack 01SK1PPED00000000000000000 --owner",
];

/// Q26b, 4.8: an unacknowledged skip is a row of the kind `skipped_event` in the section
/// `Another`, with its remedy steps and the acknowledgement; the one-line form counts it
/// and the JSON has it under `counts`. After the acknowledgement all three are gone.
#[test]
fn needs_lists_a_skipped_event_until_it_is_acknowledged() {
    let fx = Fx::new();
    fx.skip("01SK1PPED00000000000000000");

    let human = fx.text();
    let mut want = String::from(
        "\nAnother (1)\n  skipped event  01SK1PPED00000000000000000 (agent.registered, 9437500 bytes) in this \
         node's log was not sent: larger than one push\n",
    );
    for (n, step) in WORDY_STEPS.iter().enumerate() {
        let lead = if n == 0 { "    next: " } else { "          " };
        want.push_str(&format!("{lead}{step}\n"));
    }
    assert!(human.starts_with("1 needs the owner ("), "{human}");
    assert!(human.ends_with(&want), "{human}");
    assert_eq!(fx.brief(), "needs you: 1 (1 skipped event)\n");

    let v = fx.json();
    assert_eq!(v["total"], 1);
    assert_eq!(v["counts"]["skipped_event"], 1);
    let row = one(&v, "skipped_event");
    assert_eq!(
        row["key"],
        format!(
            "skipped_event:{}:01SK1PPED00000000000000000",
            fx.store.node_id()
        )
    );
    assert_eq!(row["who"], "owner");
    assert_eq!(strs(&row["next"]), WORDY_STEPS);
    assert_eq!(row["next_delegate"], json!([]));
    assert!(row["since"].is_string(), "{row}");
    assert!(row["age_seconds"].as_i64().unwrap() >= 5 * 60, "{row}");

    // More rows than the limit: the rest are counted, with the command that lists them.
    fx.skip("01SEC0ND000000000000000000");
    let cut = fx.needs(&["--limit", "1"]);
    assert!(
        cut.contains("\n  and 1 more: another activity --kinds sync\n"),
        "{cut}"
    );
    assert_eq!(fx.brief(), "needs you: 2 (2 skipped events)\n");

    for id in ["01SK1PPED00000000000000000", "01SEC0ND000000000000000000"] {
        fx.store.acknowledge_skip(id).unwrap();
    }
    assert_eq!(fx.text(), "nothing needs the owner\n");
    assert_eq!(fx.brief(), "");
    let v = fx.json();
    assert_eq!(v["counts"]["skipped_event"], 0);
    assert_eq!(v["rows"], json!([]));
}

/// The commands in `entry`: the whole entry when it is one, and the part after the colon
/// of a step that words a command (`ask ...: another send ...`).
fn command_in(entry: &str) -> Option<&str> {
    let entry = entry.trim();
    let entry = entry.strip_prefix("next: ").unwrap_or(entry);
    if entry.starts_with("another ") {
        return Some(entry);
    }
    entry.find(": another ").map(|at| &entry[at + 2..])
}

/// Split a printed command as a shell would: words, and double-quoted text as one word.
fn words(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    for c in command.chars() {
        match c {
            '"' => quoted = !quoted,
            ' ' if !quoted => {
                if !word.is_empty() {
                    out.push(std::mem::take(&mut word));
                }
            }
            c => word.push(c),
        }
    }
    assert!(!quoted, "unbalanced quote in {command:?}");
    if !word.is_empty() {
        out.push(word);
    }
    out
}

#[test]
fn every_printed_command_parses() {
    let fx = Fx::new();
    fx.everything();
    // The rest of the next steps: a gone session with a claim to release, one with mail
    // only, undelivered wakes, and a quiet session under an active name.
    fx.store
        .queue_set_limit(&QueueCaller::Owner, "bob", 3)
        .unwrap();
    let held = fx.ready("Held by a gone session");
    assert_eq!(fx.claim("bob").label, held.label);
    fx.bind(&held.label, "bob", "dead0001");
    fx.attention("bob", "dead0001", "w4:p1", "gone", "gone", 20);
    fx.set_session(
        "bob",
        "dead0001",
        &format!("hook_seen_at = {}", now_ms() - 30 * MIN),
    );
    let stale = fx.ready("Its session is quiet");
    assert_eq!(fx.claim("bob").label, stale.label);
    fx.bind(&stale.label, "bob", "slow0002");
    fx.exec(&format!(
        "UPDATE queue_sessions SET seen_at = {} WHERE session_id = 'slow0002'",
        now_ms() - 120 * MIN
    ));
    fx.attention("cy", "mail0003", "w4:p2", "gone", "gone", 20);
    fx.mail("ann", "cy", "hello", "body");
    fx.attention("dee", "undl0004", "w4:p3", "idle", "undelivered", 5);
    // Checks have stopped for every live session; the gone one with the claim, whose
    // hooks ended long ago, was checked just now.
    fx.exec(&format!(
        "UPDATE transport_sessions SET observed_at = {} WHERE session_id != 'dead0001'",
        now_ms() - 6 * MIN
    ));
    // Two skipped events (Q26b): one as this software writes it, and one whose id is not
    // an id, so no step of its row may be a bare command with a placeholder in it.
    fx.skip("01SK1PPED00000000000000000");
    fx.skip("x; id");

    let v = fx.json_with(&["--limit", "200"]);
    let human = fx.needs(&["--limit", "200"]);
    assert_eq!(rows_of(&v, "skipped_event").len(), 2);
    let mut commands: Vec<String> = Vec::new();
    for row in v["rows"].as_array().unwrap() {
        for key in ["next", "next_delegate"] {
            for entry in strs(&row[key]) {
                assert!(
                    command_in(&entry).is_some() || !entry.starts_with("another"),
                    "{entry}"
                );
                commands.extend(command_in(&entry).map(str::to_string));
            }
        }
    }
    commands.extend(human.lines().filter_map(command_in).map(str::to_string));
    commands.sort();
    commands.dedup();
    for wanted in [
        "another daemon status",
        "another doctor",
        "another transport status",
        "another wake prune --dry-run",
        "another queue list --self-admitted",
        "another inbox --agent cy",
        "another sync ack 01SK1PPED00000000000000000 --owner",
    ] {
        assert!(
            commands.iter().any(|c| c == wanted),
            "{wanted}: {commands:#?}"
        );
    }
    // This fixture has no hub, so a skip of a local event must not print a bare
    // `another sync` command (that command exits 1: this node has no hub).
    assert!(
        !commands.iter().any(|c| c == "another sync"),
        "a node with no hub must not print another sync as a command: {commands:#?}"
    );
    let skip_text = format!("{human}\n{v}");
    assert!(
        skip_text.contains("spokes receive it at their next sync"),
        "{skip_text}"
    );

    for prefix in [
        "another queue release Q6 --reason \"session gone\" --owner",
        "another queue release Q1 --reason \"<why>\" --owner",
        "another send --from <your agent> --to eve --subject ",
        "another send --from <your agent> --to bob --subject ",
        "another queue edit Q5 --for-anyone --agent <delegate>",
    ] {
        assert!(
            commands.iter().any(|c| c.starts_with(prefix)),
            "{prefix}: {commands:#?}"
        );
    }

    // clap builds the whole command tree in one frame; give it the stack the binary has.
    let checked = std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            use clap::Parser;
            for command in &commands {
                let filled = command
                    .replace("<why>", "why")
                    .replace("<delegate>", "arch")
                    .replace("<your agent>", "ann");
                assert!(!filled.contains('<'), "an unfilled placeholder: {command}");
                if let Err(e) = another::cli::Cli::try_parse_from(words(&filled)) {
                    panic!("{command} does not parse: {e}");
                }
            }
            commands.len()
        })
        .unwrap()
        .join()
        .unwrap();
    assert!(checked >= 20, "{checked} commands");
}

#[test]
fn keys_are_stable_across_runs() {
    let fx = Fx::new();
    let [claimed, urgent, proposal, reserved] = fx.everything();
    let keys = |v: &Value| -> Vec<String> {
        v["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["key"].as_str().unwrap().to_string())
            .collect()
    };
    let first = keys(&fx.json());
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert_eq!(keys(&fx.json()), first);
    assert_eq!(
        first,
        [
            "checks_stopped".to_string(),
            "transport_config".to_string(),
            format!("decide_now:{urgent}"),
            "blocked:ann:7f3a1111".to_string(),
            "stuck:bob:5c0e2222".to_string(),
            "unreachable:cy:91c03333".to_string(),
            "transport_silent:herdr".to_string(),
            format!("quiet_claim:{claimed}"),
            format!("no_taker:{reserved}"),
            format!("proposal:{proposal}"),
        ]
    );
}

#[test]
fn a_gone_session_never_offers_a_siblings_claim() {
    let fx = Fx::new();
    fx.store
        .queue_set_limit(&QueueCaller::Owner, "ann", 3)
        .unwrap();
    // Two sessions of one name. The working one holds a claim bound to itself; one more
    // claim of the name is bound to no session.
    let bound = fx.ready("Bound to the working sibling");
    assert_eq!(fx.claim("ann").label, bound.label);
    fx.bind(&bound.label, "ann", "live1111");
    let unbound = fx.ready("Bound to no session");
    assert_eq!(fx.claim("ann").label, unbound.label);
    fx.unbind(&unbound.label);
    fx.attention("ann", "live1111", "w1:p1", "working", "", 5);
    fx.set_session(
        "ann",
        "live1111",
        "attention = NULL, attention_since = NULL",
    );
    fx.attention("ann", "gone2222", "w1:p2", "gone", "gone", 30);
    fx.set_session(
        "ann",
        "gone2222",
        &format!("hook_seen_at = {}", now_ms() - 40 * MIN),
    );

    // With nothing waiting on it, the gone session is only in the closing line.
    let v = fx.json();
    assert_eq!(v["total"], 0, "{v:#}");
    assert_eq!(v["closing"]["gone_idle"], 1);
    let human = fx.text();
    assert!(
        human.contains("1 session gone with nothing waiting"),
        "{human}"
    );

    // Mail for the name makes it a row. The row still has no claim of its own: the bound
    // one is the sibling's, and the unbound one is only named.
    fx.mail("bob", "ann", "hello", "body");
    let v = fx.json();
    assert_eq!(v["total"], 1, "{v:#}");
    assert_eq!(v["closing"]["gone_idle"], 0);
    let row = one(&v, "unreachable");
    assert_eq!(row["reason"], "gone");
    assert_eq!(row["session"], "gone2222");
    assert_eq!(row["claims"], json!([]));
    assert_eq!(row["name_claims"], json!([unbound.label]));
    assert_eq!(strs(&row["next"]), ["another inbox --agent ann"]);
    assert_eq!(row["next_delegate"], json!([]));
    let human = fx.text();
    assert!(
        human.contains(&format!("name also holds {} (not bound)", unbound.label)),
        "{human}"
    );
    for out in [human, v.to_string(), fx.brief()] {
        assert!(!out.contains("queue release"), "{out}");
        assert!(!out.contains("wake clear"), "{out}");
        assert!(!out.contains(&format!("claims {}", bound.label)), "{out}");
    }
}

#[test]
fn a_gone_session_offers_only_its_own_bound_claim() {
    let fx = Fx::new();
    fx.store
        .queue_set_limit(&QueueCaller::Owner, "ann", 3)
        .unwrap();
    let own = fx.ready("Held by the gone session");
    assert_eq!(fx.claim("ann").label, own.label);
    fx.bind(&own.label, "ann", "gone1111");
    let other = fx.ready("Held by its sibling");
    assert_eq!(fx.claim("ann").label, other.label);
    fx.bind(&other.label, "ann", "live2222");
    fx.attention("ann", "live2222", "w1:p1", "working", "", 5);
    fx.set_session(
        "ann",
        "live2222",
        "attention = NULL, attention_since = NULL",
    );

    // Gone for 20 minutes, no hook since, seen live before, checked just now.
    let baseline = || {
        fx.attention("ann", "gone1111", "w1:p2", "gone", "gone", 20);
        fx.set_session(
            "ann",
            "gone1111",
            &format!("hook_seen_at = {}", now_ms() - 30 * MIN),
        );
    };
    let show = format!("another queue show {}", own.label);
    let release = format!(
        "another queue release {} --reason \"session gone\" --owner",
        own.label
    );
    let wait = "wait: the session may come back";

    baseline();
    let v = fx.json();
    assert_eq!(v["total"], 1, "{v:#}");
    let row = one(&v, "unreachable");
    assert_eq!(row["key"], "unreachable:ann:gone1111");
    assert_eq!(row["who"], "owner");
    assert_eq!(row["reason"], "gone");
    assert_eq!(row["agent"], "ann");
    assert_eq!(row["transport"], "herdr");
    assert_eq!(row["target"], "w1:p2");
    assert_eq!(row["claims"], json!([own.label]));
    assert_eq!(row["name_claims"], json!([]));
    assert_eq!(strs(&row["next"]), [show.clone(), release.clone()]);
    assert_eq!(row["next_delegate"], json!([]));
    assert!(row["age_seconds"].as_i64().unwrap() >= 20 * 60);
    let human = fx.text();
    assert!(
        human.contains(&format!(
            "  unreachable  ann gone..  herdr w1:p2  gone 20m  claims {}\n",
            own.label
        )),
        "{human}"
    );
    assert!(!human.contains(&other.label), "{human}");

    let next = || strs(&one(&fx.json(), "unreachable")["next"]);
    // The session keeps stamping hooks: it is alive, whatever the listing says.
    fx.set_session("ann", "gone1111", &format!("hook_seen_at = {}", now_ms()));
    assert_eq!(next(), [show.as_str(), wait]);
    // Gone for under 15 minutes.
    baseline();
    fx.set_session(
        "ann",
        "gone1111",
        &format!("state_since = {}", now_ms() - 14 * MIN),
    );
    assert_eq!(next(), [show.as_str(), wait]);
    // Never seen live by a trusted listing.
    baseline();
    fx.set_session("ann", "gone1111", "seen_live_at = NULL");
    assert_eq!(next(), [show.as_str(), wait]);
    // Not checked in the last five minutes.
    baseline();
    fx.set_session(
        "ann",
        "gone1111",
        &format!("observed_at = {}", now_ms() - 6 * MIN),
    );
    assert_eq!(next(), [show.as_str(), wait]);
    assert!(fx.text().contains("  as of "), "{}", fx.text());
    // Back at the baseline the release is offered again, and only for its own item.
    baseline();
    assert_eq!(next(), [show, release]);
    assert!(!fx.text().contains(&format!("release {}", other.label)));
}

#[test]
fn no_agent_row_prints_a_name_wide_command() {
    let fx = Fx::new();
    fx.store
        .queue_set_limit(&QueueCaller::Owner, "ann", 3)
        .unwrap();
    let held = fx.ready("Held by a gone session");
    assert_eq!(fx.claim("ann").label, held.label);
    fx.bind(&held.label, "ann", "gone1111");
    let loose = fx.ready("Bound to no session");
    assert_eq!(fx.claim("ann").label, loose.label);
    fx.unbind(&loose.label);
    fx.attention("ann", "gone1111", "w1:p1", "gone", "gone", 20);
    fx.set_session(
        "ann",
        "gone1111",
        &format!("hook_seen_at = {}", now_ms() - 30 * MIN),
    );
    fx.attention("ann", "blok2222", "w1:p2", "blocked", "blocked", 3);
    fx.attention("bob", "stuk3333", "w1:p3", "idle", "stuck", 3);
    let now = now_ms();
    fx.set_session(
        "bob",
        "stuk3333",
        &format!(
            "wake_stop = 'unknown', wake_stop_at = {now}, last_wake_at = {now},
             last_wake_outcome = 'unknown', hook_seen_at = {}",
            now - MIN
        ),
    );
    fx.attention("bob", "gone4444", "w1:p4", "gone", "gone", 20);
    fx.mail("ann", "bob", "hello", "body");
    fx.attention("cy", "unkn5555", "w1:p5", "unknown", "unknown", 11);
    fx.attention("cy", "undl6666", "w1:p6", "idle", "undelivered", 2);
    fx.attention("dee", "none7777", "w1:p7", "no_answer", "no_answer", 11);

    let v = fx.json_with(&["--limit", "200"]);
    let agent_kinds = ["blocked", "stuck", "unreachable", "transport_silent"];
    let rows: Vec<&Value> = v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| agent_kinds.contains(&r["kind"].as_str().unwrap()))
        .collect();
    let seen: Vec<(String, String)> = rows
        .iter()
        .map(|r| {
            (
                r["kind"].as_str().unwrap().to_string(),
                r["reason"].as_str().unwrap_or("-").to_string(),
            )
        })
        .collect();
    for wanted in [
        ("blocked", "blocked"),
        ("stuck", "unknown"),
        ("unreachable", "gone"),
        ("unreachable", "unknown"),
        ("unreachable", "undelivered"),
        ("transport_silent", "no_answer"),
    ] {
        assert!(
            seen.iter().any(|(k, r)| (k.as_str(), r.as_str()) == wanted),
            "{wanted:?} in {seen:?}"
        );
    }
    assert_eq!(rows.len(), 7, "{seen:?}");
    for row in rows {
        for key in ["next", "next_delegate"] {
            for entry in strs(&row[key]) {
                assert!(!entry.contains("wake clear"), "{row:#}");
                assert!(!entry.contains("wake --agent"), "{row:#}");
            }
        }
        // The name's unbound claim gets no command on an agent row.
        assert!(!row["next"].to_string().contains(&loose.label), "{row:#}");
    }
    let human = fx.needs(&["--limit", "200"]);
    assert!(!human.contains("wake clear"), "{human}");
    assert!(!human.contains("wake --agent"), "{human}");
}

#[test]
fn a_stuck_row_goes_when_its_hook_ran() {
    let fx = Fx::new();
    fx.session("bob", "5c0e2222", "herdr", "w1:p3");
    fx.set_session(
        "bob",
        "5c0e2222",
        &format!(
            "state = 'idle', state_since = {now}, observed_at = {now}, matched = 1",
            now = now_ms()
        ),
    );
    // A delivered wake that started nothing: the failed wake of M7.
    fx.store
        .record_transport_wake(&TransportWake {
            agent: "bob".into(),
            session_id: Some("5c0e2222".into()),
            transport: "herdr".into(),
            target: "w1:p3".into(),
            outcome: WakeOutcome::NotStarted,
            detail: "no activity seen".into(),
            kind: WakeKind::Mail,
            message_ids: Vec::new(),
            trigger: "manual".into(),
        })
        .unwrap();

    let v = fx.json();
    assert_eq!(v["total"], 1, "{v:#}");
    let row = one(&v, "stuck");
    assert_eq!(row["key"], "stuck:bob:5c0e2222");
    assert_eq!(row["who"], "owner");
    assert_eq!(row["reason"], "not_started");
    assert_eq!(
        strs(&row["next"]),
        ["in herdr pane w1:p3, clear the input box, then send the agent any prompt"]
    );
    assert_eq!(row["next_delegate"], json!([]));
    let human = fx.text();
    assert!(
        human.contains("  stuck        bob 5c0e..  herdr w1:p3  ")
            && human.contains("  last wake: delivered, did not start"),
        "{human}"
    );

    // The owner sends any prompt in that pane: the session's hook runs. No sweep follows.
    std::thread::sleep(std::time::Duration::from_millis(5));
    fx.session("bob", "5c0e2222", "herdr", "w1:p3");
    let v = fx.json();
    assert_eq!(v["total"], 0, "{v:#}");
    assert_eq!(fx.text(), "nothing needs the owner\n");
    // The stored reason has not changed: the list hides the row by the stop rule alone.
    let stored: String = fx
        .db()
        .query_row("SELECT attention FROM transport_sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, "stuck");
}

#[test]
fn blocked_unknown_and_undelivered_are_listed() {
    let fx = Fx::new();
    fx.attention("ann", "7f3a1111", "w1:p1", "blocked", "blocked", 12);
    fx.attention("bob", "91c02222", "w1:p2", "unknown", "unknown", 30);
    fx.attention("cy", "33aa3333", "w1:p3", "idle", "undelivered", 3);
    fx.set_session("cy", "33aa3333", "undelivered_count = 3");

    let v = fx.json();
    assert_eq!(v["total"], 3, "{v:#}");
    assert_eq!(v["counts"]["blocked"], 1);
    assert_eq!(v["counts"]["unreachable"], 2);
    let row = one(&v, "blocked");
    assert_eq!(row["who"], "owner");
    assert_eq!(row["agent"], "ann");
    assert_eq!(row["session"], "7f3a1111");
    assert_eq!(row["reason"], "blocked");
    assert_eq!(
        strs(&row["next"]),
        ["answer the dialog in herdr pane w1:p1"]
    );
    assert!(row["since"].is_string());
    assert!(row["as_of"].is_string());
    assert!((12 * 60..13 * 60).contains(&row["age_seconds"].as_i64().unwrap()));

    let rows = rows_of(&v, "unreachable");
    let by_reason = |reason: &str| {
        rows.iter()
            .find(|r| r["reason"] == reason)
            .unwrap_or_else(|| panic!("{reason} in {rows:#?}"))
    };
    assert_eq!(
        strs(&by_reason("unknown")["next"]),
        ["look at herdr pane w1:p2: it may hold another session"]
    );
    assert_eq!(
        strs(&by_reason("undelivered")["next"]),
        ["another doctor", "another transport status"]
    );
    // Oldest first inside the kind.
    assert_eq!(rows[0]["reason"], "unknown");

    let human = fx.text();
    for line in [
        "  blocked      ann 7f3a..  herdr w1:p1  12m\n",
        "  unreachable  bob 91c0..  herdr w1:p2  unknown 30m\n",
        "  unreachable  cy 33aa..  herdr w1:p3  undelivered 3m\n",
    ] {
        assert!(human.contains(line), "{line:?} in:\n{human}");
    }
}

#[test]
fn a_silent_transport_is_one_row() {
    let fx = Fx::new();
    for (n, agent) in ["ann", "bob", "cy", "dee", "eve"].iter().enumerate() {
        fx.attention(
            agent,
            &format!("sess000{n}"),
            &format!("w1:p{n}"),
            "no_answer",
            "no_answer",
            12 + n as i64,
        );
    }
    let v = fx.json();
    assert_eq!(v["total"], 1, "{v:#}");
    assert_eq!(v["counts"]["transport_silent"], 1);
    let row = one(&v, "transport_silent");
    assert_eq!(row["key"], "transport_silent:herdr");
    assert_eq!(row["who"], "owner");
    assert_eq!(row["transport"], "herdr");
    for key in ["agent", "session", "target", "item"] {
        assert_eq!(row[key], Value::Null, "{key}");
    }
    assert_eq!(row["claims"], json!([]));
    assert_eq!(row["name_claims"], json!([]));
    assert_eq!(strs(&row["next"]), ["another doctor", "herdr agent list"]);
    let human = fx.text();
    assert!(
        human.contains("  transport silent  herdr has not answered for 16m, 5 sessions\n"),
        "{human}"
    );
    assert_eq!(fx.brief(), "needs you: 1 (1 transport silent)\n");
}

#[test]
fn only_a_live_unchecked_herdr_session_says_checks_stopped() {
    let raised = |fx: &Fx| fx.json()["counts"]["checks_stopped"] == 1;
    let hooks_at = |fx: &Fx, sid: &str, mins: i64| {
        fx.set_session(
            "ann",
            sid,
            &format!("hook_seen_at = {}", now_ms() - mins * MIN),
        );
    };
    let observed_at = |fx: &Fx, sid: &str, mins: i64| {
        fx.set_session(
            "ann",
            sid,
            &format!("observed_at = {}", now_ms() - mins * MIN),
        );
    };

    // A live Herdr session nobody checked for five minutes.
    let fx = Fx::new();
    fx.session("ann", "aaaa1111", "herdr", "w1:p1");
    observed_at(&fx, "aaaa1111", 6);
    let v = fx.json();
    assert_eq!(v["total"], 1, "{v:#}");
    let row = one(&v, "checks_stopped");
    assert_eq!(row["key"], "checks_stopped");
    assert_eq!(row["who"], "owner");
    assert_eq!(strs(&row["next"]), ["another daemon status"]);
    let human = fx.text();
    assert!(
        human.contains("  checks stopped  1 live Herdr session, none checked since 20")
            && human.contains("; agent rows may be out of date\n"),
        "{human}"
    );
    assert_eq!(fx.brief(), "needs you: 1 (checks stopped)\n");
    // A fresh observation clears it.
    observed_at(&fx, "aaaa1111", 0);
    assert!(!raised(&fx));
    // Its hooks stopped ten minutes ago: nothing live is left to check.
    observed_at(&fx, "aaaa1111", 30);
    hooks_at(&fx, "aaaa1111", 11);
    assert!(!raised(&fx));

    // Never checked at all, with two sessions.
    let fx = Fx::new();
    fx.session("ann", "aaaa1111", "herdr", "w1:p1");
    fx.session("bob", "bbbb2222", "herdr", "w1:p2");
    assert!(raised(&fx));
    assert!(
        fx.text()
            .contains("2 live Herdr sessions, none checked since ever; "),
        "{}",
        fx.text()
    );
    // A session that has just started beside one that was just checked raises nothing.
    observed_at(&fx, "aaaa1111", 0);
    assert!(!raised(&fx));

    // A wake target set by hand, alone.
    let fx = Fx::new();
    let out = fx.run(&[
        "wake", "set", "--agent", "ann", "--via", "herdr", "--target", "w1:p1",
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(!raised(&fx));
    assert_eq!(fx.text(), "nothing needs the owner\n");

    // A row of a command transport, never checked.
    let fx = Fx::new();
    fx.session("ann", "aaaa1111", "tmux", "%3");
    assert!(!raised(&fx));
    assert_eq!(fx.text(), "nothing needs the owner\n");
}

#[test]
fn a_refused_transports_file_is_a_row() {
    let fx = Fx::new();
    transports::write(&fx.home, "{ not json");
    let v = fx.json();
    assert_eq!(v["total"], 1, "{v:#}");
    let row = one(&v, "transport_config");
    assert_eq!(row["key"], "transport_config");
    assert_eq!(row["who"], "owner");
    assert!(
        row["reason"].as_str().unwrap().contains("transports.json"),
        "{row:#}"
    );
    assert_eq!(strs(&row["next"]), ["another doctor"]);
    let human = fx.text();
    assert!(human.contains("\nAnother (1)\n"), "{human}");
    assert!(human.contains("  transports.json refused  "), "{human}");
    assert_eq!(fx.brief(), "needs you: 1 (transports.json refused)\n");

    transports::write_example(&fx.home);
    assert_eq!(fx.json()["total"], 0);
    assert_eq!(fx.text(), "nothing needs the owner\n");
}

#[test]
fn gone_sessions_with_nothing_waiting_are_one_closing_line() {
    let fx = Fx::new();
    fx.attention("ann", "aaaa1111", "w1:p1", "gone", "gone", 30);
    fx.attention("bob", "bbbb2222", "w1:p2", "gone", "gone", 40);
    fx.attention("cy", "cccc3333", "w1:p3", "gone", "gone", 50);
    let v = fx.json();
    assert_eq!(v["total"], 0, "{v:#}");
    assert_eq!(v["rows"], json!([]));
    assert_eq!(v["closing"]["gone_idle"], 3);
    assert_eq!(
        fx.text(),
        "nothing needs the owner\n\n3 sessions gone with nothing waiting: another wake prune --dry-run\n"
    );
    assert_eq!(fx.brief(), "");

    // Mail no channel has announced makes that one session a counted row.
    fx.mail("ann", "cy", "hello", "body");
    let v = fx.json();
    assert_eq!(v["total"], 1, "{v:#}");
    assert_eq!(v["closing"]["gone_idle"], 2);
    let row = one(&v, "unreachable");
    assert_eq!(row["agent"], "cy");
    assert_eq!(strs(&row["next"]), ["another inbox --agent cy"]);
    assert_eq!(fx.brief(), "needs you: 1 (1 unreachable)\n");

    // Two more gone sessions of that agent: the mail is still one row and one inbox step.
    fx.attention("cy", "dddd4444", "w1:p4", "gone", "gone", 20);
    fx.attention("cy", "eeee5555", "w1:p5", "gone", "gone", 10);
    let v = fx.json();
    assert_eq!(v["total"], 1, "{v:#}");
    assert_eq!(v["closing"]["gone_idle"], 4);
    assert_eq!(one(&v, "unreachable")["key"], "unreachable:cy:cccc3333");
    assert_eq!(fx.text().matches("another inbox --agent cy").count(), 1);
    assert_eq!(fx.brief(), "needs you: 1 (1 unreachable)\n");
}

#[test]
fn failed_notices_are_a_closing_line() {
    let fx = Fx::new();
    let item = fx.ready("Its notice failed");
    assert_eq!(fx.claim("bob").label, item.label);
    fx.failed_notice(&item, "bob");
    let v = fx.json();
    assert_eq!(v["total"], 0, "{v:#}");
    let notices = v["closing"]["failed_notices"].as_array().unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0]["item"], item.label);
    assert_eq!(notices[0]["to"], "bob");
    assert!(notices[0]["at"].is_string());
    assert_eq!(
        fx.text(),
        format!(
            "nothing needs the owner\n\n1 notice could not be sent in the last 24 hours ({q} to bob): another queue show {q}\n",
            q = item.label
        )
    );
    assert_eq!(fx.brief(), "");

    // Past five, the rest are counted.
    for _ in 0..6 {
        fx.failed_notice(&item, "bob");
    }
    let human = fx.text();
    assert!(
        human.contains("7 notices could not be sent in the last 24 hours (")
            && human.contains(" and 2 more): another queue show "),
        "{human}"
    );
}

#[test]
fn self_admitted_names_its_items_and_is_not_counted() {
    let fx = Fx::new();
    let arch = fx.delegate();
    let own = fx
        .store
        .queue_propose(
            &arch,
            ProposeItem {
                title: "Tidy the docs".into(),
                scope: Some("M7".into()),
                done_when: Some("merged".into()),
                ..Default::default()
            },
        )
        .unwrap();
    fx.store
        .queue_admit(&arch, &own.label, AdmitItem::default())
        .unwrap();
    let v = fx.json();
    assert_eq!(v["total"], 0, "{v:#}");
    assert_eq!(
        v["closing"]["self_admitted"],
        json!({ "recent": [own.label], "total": 1 })
    );
    assert_eq!(
        fx.text(),
        format!(
            "nothing needs the owner\n\n1 self-admitted in the last 7 days ({}), 1 in all: another queue list --self-admitted\n",
            own.label
        )
    );
    assert_eq!(fx.brief(), "");
}

#[test]
fn quiet_claim_commands_follow_the_evidence() {
    // Reason 1: nothing from the whole name for 95 minutes.
    let fx = Fx::new();
    let item = fx.ready("Hold fixes");
    assert_eq!(fx.claim("cy").label, item.label);
    fx.quiet("cy", 95);
    let q = item.label.as_str();
    let show = format!("another queue show {q}");
    let ask = format!(
        "ask cy whether a session is working on {q}: another send --from <your agent> --to cy --subject \"Who holds {q}?\""
    );
    let v = fx.json();
    assert_eq!(v["total"], 1, "{v:#}");
    let row = one(&v, "quiet_claim");
    assert_eq!(row["key"], format!("quiet_claim:{q}"));
    assert_eq!(row["who"], "admitter");
    assert_eq!(row["reason"], "name_quiet");
    assert_eq!(row["agent"], "cy");
    assert_eq!(row["item"], q);
    assert_eq!(
        strs(&row["next"]),
        [
            show.clone(),
            ask.clone(),
            format!("another queue release {q} --reason \"<why>\" --owner"),
        ]
    );
    assert_eq!(
        strs(&row["next_delegate"]),
        [
            show.clone(),
            ask.clone(),
            format!("another queue release {q} --reason \"<why>\" --agent <delegate>"),
        ]
    );
    let human = fx.text();
    assert!(
        human.contains(&format!(
            "  {q} M7  Hold fixes  claimed by cy, quiet for 95 min, not bound to a session, grace ends "
        )),
        "{human}"
    );
    // `queue list --needs-owner` words it the same way and gives the reason.
    let out = fx.run(&["queue", "list", "--needs-owner"]);
    assert!(
        text(&out.stdout).contains("quiet for 95 min, not bound to a session, grace ends "),
        "{}",
        text(&out.stdout)
    );
    let out = fx.run(&["--json", "queue", "list", "--needs-owner"]);
    let list: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(list["quiet_claims"][0]["reason"], "name_quiet");
    assert_eq!(
        list["quiet_claims"][0]["session_quiet_for_seconds"],
        Value::Null
    );

    // A session of the name is working now: no release.
    fx.attention("cy", "work1111", "w1:p1", "working", "", 1);
    fx.set_session("cy", "work1111", "attention = NULL, attention_since = NULL");
    // The transport row is all that shows life; the name's own last sign stays old.
    fx.quiet("cy", 95);
    let row = one(&fx.json(), "quiet_claim");
    assert_eq!(row["reason"], "name_quiet");
    assert_eq!(strs(&row["next"]), [show.clone(), ask.clone()]);
    assert_eq!(strs(&row["next_delegate"]), [show.clone(), ask.clone()]);

    // An observation licenses a release only while it is fresh. A session of the name
    // seen idle and checked just now: the release is printed.
    let full = |fx: &Fx| {
        let next = strs(&one(&fx.json(), "quiet_claim")["next"]);
        assert_eq!(next[..2], [show.clone(), ask.clone()], "{next:?}");
        match next.len() {
            2 => false,
            3 => {
                assert!(next[2].starts_with("another queue release "), "{next:?}");
                true
            }
            _ => panic!("{next:?}"),
        }
    };
    fx.set_session("cy", "work1111", "state = 'idle'");
    assert!(full(&fx));
    // Seen and not identified, or there with no state reported: the pane may hold the
    // session in a turn, or someone else. Ask only.
    for unclear in ["unknown", "present", "blocked", "no_answer"] {
        fx.set_session("cy", "work1111", &format!("state = '{unclear}'"));
        assert!(!full(&fx), "{unclear}");
    }
    // Gone and idle sessions, all checked just now, leave the release.
    fx.set_session("cy", "work1111", "state = 'gone'");
    assert!(full(&fx));
    fx.set_session("cy", "work1111", "state = 'idle'");
    // Checks stopped: the row was last seen idle two hours ago, and the session may be
    // inside one long tool call. Only the ask step is printed.
    let two_hours_ago = now_ms() - 120 * MIN;
    fx.set_session(
        "cy",
        "work1111",
        &format!("observed_at = {two_hours_ago}, state_since = {two_hours_ago}"),
    );
    let row = one(&fx.json(), "quiet_claim");
    assert_eq!(strs(&row["next"]), [show.clone(), ask.clone()]);
    assert_eq!(strs(&row["next_delegate"]), [show.clone(), ask.clone()]);
    // One stale row of the name is enough, beside a fresh idle one.
    fx.attention("cy", "idle2222", "w1:p2", "idle", "", 1);
    fx.set_session("cy", "idle2222", "attention = NULL, attention_since = NULL");
    fx.quiet("cy", 95);
    assert!(!full(&fx));
    // A row a hook made that was never checked says nothing either.
    fx.exec("DELETE FROM transport_sessions");
    assert!(full(&fx));
    fx.session("cy", "new03333", "tmux", "%3");
    fx.quiet("cy", 95);
    assert!(!full(&fx));

    // A row that only holds the outcome of a wake by hand is not a session: the sweep
    // never checks it, and it does not hide the release.
    fx.exec("DELETE FROM transport_sessions");
    let out = fx.run(&[
        "wake", "set", "--agent", "cy", "--via", "herdr", "--target", "w9:p9",
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    fx.store
        .record_transport_wake(&TransportWake {
            agent: "cy".into(),
            session_id: None,
            transport: "herdr".into(),
            target: "w9:p9".into(),
            outcome: WakeOutcome::Unconfirmed,
            detail: "sent".into(),
            kind: WakeKind::Mail,
            message_ids: Vec::new(),
            trigger: "manual".into(),
        })
        .unwrap();
    let made: (i64, Option<i64>, Option<i64>) = fx
        .db()
        .query_row(
            "SELECT count(*), max(hook_seen_at), max(observed_at) FROM transport_sessions",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(made, (1, None, None), "one row, made by the wake alone");
    fx.quiet("cy", 95);
    assert!(full(&fx));

    // Reason 2: the name is active and the claim's own session has been quiet.
    let fx = Fx::new();
    let item = fx.ready("Bound to a quiet session");
    assert_eq!(fx.claim("cy").label, item.label);
    fx.bind(&item.label, "cy", "slow1111");
    fx.exec(&format!(
        "UPDATE queue_sessions SET seen_at = {}",
        now_ms() - 125 * MIN
    ));
    let q = item.label.as_str();
    let row = one(&fx.json(), "quiet_claim");
    assert_eq!(row["reason"], "session_quiet");
    assert_eq!(row["who"], "admitter");
    let steps = [
        format!("another queue show {q}"),
        format!(
            "ask cy whether a session is working on {q}: another send --from <your agent> --to cy --subject \"Who holds {q}?\""
        ),
    ];
    assert_eq!(strs(&row["next"]), steps);
    assert_eq!(strs(&row["next_delegate"]), steps);
    let human = fx.text();
    assert!(
        human.contains(
            "claimed by cy, name active, its session quiet for 125 min, its session last seen 20"
        ),
        "{human}"
    );
    assert!(!human.contains("wake --agent"), "{human}");
    assert!(!human.contains("queue release"), "{human}");
    let out = fx.run(&["--json", "queue", "list", "--needs-owner"]);
    let list: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(list["quiet_claims"][0]["reason"], "session_quiet");

    // Reason 3: the name is active and the unbound claim is over a grace period old.
    let fx = Fx::new();
    let item = fx.ready("Unbound for over a day");
    assert_eq!(fx.claim("cy").label, item.label);
    fx.exec(&format!(
        "UPDATE queue_items SET claimed_at = {}",
        now_ms() - 25 * 60 * MIN
    ));
    let q = item.label.as_str();
    let row = one(&fx.json(), "quiet_claim");
    assert_eq!(row["reason"], "old_unbound");
    assert!(row["age_seconds"].as_i64().unwrap() >= 25 * 3600);
    let steps = [
        format!("another queue show {q}"),
        format!(
            "ask cy whether a session is working on {q}: another send --from <your agent> --to cy --subject \"Who holds {q}?\""
        ),
    ];
    assert_eq!(strs(&row["next"]), steps);
    assert_eq!(strs(&row["next_delegate"]), steps);
    let human = fx.text();
    assert!(
        human.contains("claimed by cy, name active, claimed 1d ago, not bound to a session\n"),
        "{human}"
    );
    assert!(!human.contains("wake --agent"), "{human}");
    assert!(!human.contains("queue release"), "{human}");

    // A fast-lane item is the owner's, whatever the reason.
    let fx = Fx::new();
    let item = fx.ready_as("A leak", QueueKind::Security, AdmitItem::default());
    assert_eq!(fx.claim("cy").label, item.label);
    fx.quiet("cy", 61);
    let row = one(&fx.json(), "quiet_claim");
    assert_eq!(row["who"], "owner");
    assert_eq!(row["item_kind"], "security");
    assert_eq!(row["next_delegate"], json!([]));
    assert_eq!(strs(&row["next"]).len(), 3);
}

#[test]
fn no_output_prints_a_wake() {
    let fx = Fx::new();
    fx.everything();
    // Beside one row of every kind: a quiet claim of each reason and a gone session
    // with a claim to release.
    fx.store
        .queue_set_limit(&QueueCaller::Owner, "bob", 3)
        .unwrap();
    let held = fx.ready("Held by a gone session");
    assert_eq!(fx.claim("bob").label, held.label);
    fx.bind(&held.label, "bob", "dead0001");
    fx.attention("bob", "dead0001", "w4:p1", "gone", "gone", 20);
    let stale = fx.ready("Its session is quiet");
    assert_eq!(fx.claim("bob").label, stale.label);
    fx.bind(&stale.label, "bob", "slow0002");
    fx.exec(&format!(
        "UPDATE queue_sessions SET seen_at = {} WHERE session_id = 'slow0002'",
        now_ms() - 120 * MIN
    ));
    let old = fx.ready("Unbound for over a day");
    assert_eq!(fx.claim("cy").label, old.label);
    fx.exec(&format!(
        "UPDATE queue_items SET claimed_at = {} WHERE seq = {}",
        now_ms() - 25 * 60 * MIN,
        old.seq
    ));

    let v = fx.json_with(&["--limit", "200"]);
    let reasons: Vec<&str> = rows_of(&v, "quiet_claim")
        .iter()
        .map(|r| r["reason"].as_str().unwrap().to_string())
        .map(|r| match r.as_str() {
            "name_quiet" => "name_quiet",
            "session_quiet" => "session_quiet",
            _ => "old_unbound",
        })
        .collect();
    for reason in ["name_quiet", "session_quiet", "old_unbound"] {
        assert!(reasons.contains(&reason), "{reason} in {reasons:?}");
    }
    // The quiet name's claim keeps its release; no step of any row is a wake.
    assert!(v
        .to_string()
        .contains("another queue release Q1 --reason \\\"<why>\\\" --owner"));
    for row in v["rows"].as_array().unwrap() {
        for key in ["next", "next_delegate"] {
            for step in strs(&row[key]) {
                assert!(!step.contains("another wake"), "{row:#}");
                assert!(!step.contains("--force"), "{row:#}");
            }
        }
    }
    // The one place the word stands is the closing line's read-only prune, which asks
    // no transport and types nothing. Nothing else in any form names the command.
    let prune = "another wake prune --dry-run";
    let human = fx.needs(&["--limit", "200"]);
    assert_eq!(human.matches(prune).count(), 1, "{human}");
    for out in [
        human,
        fx.text(),
        fx.brief(),
        v.to_string(),
        fx.needs(&["--json", "--brief"]),
    ] {
        let rest = out.replace(prune, "");
        assert!(!rest.contains("another wake"), "{out}");
        assert!(!rest.contains("wake --"), "{out}");
        assert!(!out.contains("--force"), "{out}");
    }
}

/// The reviewers' cases: the claimer's name is quiet and another agent now works in its
/// pane, on a transport that can type and cannot say who is in a pane. Whatever the
/// claimer's own row says, the list prints no wake, and running every command it does
/// print never runs the pane tool.
#[cfg(unix)]
#[test]
fn no_printed_step_types_into_another_agents_pane() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fx::new();
    let root = fx.home.parent().unwrap().to_path_buf();
    let log = root.join("typed.log");
    let script = root.join("fake-pane-tool");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 0\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let program = script.to_string_lossy().into_owned();
    // A transport that can type and can say nothing about who is in a pane.
    transports::write(
        &fx.home,
        &json!({ "version": 1, "transports": { "fake": {
            "wake": [[program, "send", "{target}", "{message}"]]
        }}})
        .to_string(),
    );
    let item = fx.ready("Hold fixes");
    assert_eq!(fx.claim("eve").label, item.label);
    let now = now_ms();
    let quiet_at = now - 120 * MIN;
    fx.session("eve", "s-eve001", "fake", "%5");
    fx.session("ann", "s-ann001", "fake", "%5");
    fx.set_session(
        "ann",
        "s-ann001",
        &format!("state = 'working', state_since = {now}, observed_at = {now}, matched = 1"),
    );
    // Eve's row as a check leaves it: the pane lists another session's id (`unknown`),
    // the pane is listed with no id (`present`), or the session is not listed (`gone`).
    for (state, attention, releases) in [
        ("unknown", "'unknown'", false),
        ("present", "NULL", false),
        ("gone", "'gone'", true),
    ] {
        fx.set_session(
            "eve",
            "s-eve001",
            &format!(
                "state = '{state}', state_since = {quiet_at}, observed_at = {now},
                 hook_seen_at = {quiet_at}, matched = 0, attention = {attention},
                 attention_since = {quiet_at}"
            ),
        );
        fx.quiet("eve", 120);
        let v = fx.json();
        let row = one(&v, "quiet_claim");
        assert_eq!(row["reason"], "name_quiet", "{state}");
        let mut commands = Vec::new();
        for all in v["rows"].as_array().unwrap() {
            for key in ["next", "next_delegate"] {
                for step in strs(&all[key]) {
                    assert!(!step.contains("another wake"), "{state}: {step}");
                    commands.extend(command_in(&step).map(str::to_string));
                }
            }
        }
        assert_eq!(
            commands.iter().any(|c| c.contains("queue release")),
            releases,
            "{state}: {commands:?}"
        );
        assert!(commands.len() >= 3, "{state}: {commands:?}");
        // Run each printed command as the owner or a delegate would paste it.
        for command in &commands {
            let filled = command
                .replace("<why>", "why")
                .replace("<delegate>", "arch")
                .replace("<your agent>", "ann");
            let argv = words(&filled);
            let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
            fx.run(&args);
            assert!(
                !log.exists(),
                "{state}: `{command}` ran the pane tool: {}",
                std::fs::read_to_string(&log).unwrap_or_default()
            );
        }
    }

    // The wake the list used to print for this claim is now refused by the wake itself
    // (D95): eve's row reads `unknown`, and ann is working in the pane.
    fx.set_session("eve", "s-eve001", "state = 'unknown'");
    let message = ["--message", "Q1 has been quiet"];
    let out = fx.run(&[&["wake", "--agent", "eve"][..], &message].concat());
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    assert!(!log.exists(), "the refused wake ran the pane tool");
    // The control: the pane tool does run when a wake types, here with the pane named.
    let named = ["wake", "--agent", "eve", "--via", "fake", "--target", "%5"];
    fx.run(&[&named[..], &message].concat());
    let typed = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(typed.contains("send %5 Q1 has been quiet"), "{typed:?}");
}

#[test]
fn no_taker_names_the_right_command() {
    // Reserved for an agent that cannot take it now.
    let fx = Fx::new();
    let item = fx.ready_as(
        "Reserved",
        QueueKind::Work,
        AdmitItem {
            for_agent: Some("bob".into()),
            ..Default::default()
        },
    );
    fx.quiet("bob", 61);
    let q = item.label.as_str();
    let v = fx.json();
    assert_eq!(v["total"], 1, "{v:#}");
    let row = one(&v, "no_taker");
    assert_eq!(row["key"], format!("no_taker:{q}"));
    assert_eq!(row["who"], "admitter");
    assert_eq!(row["reason"], "reserved");
    assert_eq!(row["agent"], "bob");
    assert_eq!(row["since"], Value::Null);
    assert_eq!(row["age_seconds"], Value::Null);
    assert_eq!(
        strs(&row["next"]),
        [format!("another queue edit {q} --for-anyone --owner")]
    );
    assert_eq!(
        strs(&row["next_delegate"]),
        [format!(
            "another queue edit {q} --for-anyone --agent <delegate>"
        )]
    );
    assert!(
        fx.text().contains(&format!(
            "  {q} M7  Reserved  ready, reserved for bob, which cannot take it now\n"
        )),
        "{}",
        fx.text()
    );
    // A reserved fast-lane item is the owner's.
    let fx = Fx::new();
    fx.ready_as(
        "Reserved leak",
        QueueKind::Security,
        AdmitItem {
            for_agent: Some("bob".into()),
            ..Default::default()
        },
    );
    fx.quiet("bob", 61);
    let row = one(&fx.json(), "no_taker");
    assert_eq!(row["who"], "owner");
    assert_eq!(row["next_delegate"], json!([]));

    // The default limit is 0 and nobody has a limit of their own.
    let fx = Fx::new();
    fx.store
        .queue_set_default_limit(&QueueCaller::Owner, 0)
        .unwrap();
    let item = fx.ready("No seats by default");
    let row = one(&fx.json(), "no_taker");
    assert_eq!(row["who"], "owner");
    assert_eq!(row["reason"], "no_seat");
    assert_eq!(row["agent"], Value::Null);
    assert_eq!(
        strs(&row["next"]),
        ["another queue limit default 1 --owner"]
    );
    assert_eq!(row["next_delegate"], json!([]));
    assert!(
        fx.text().contains(&format!(
            "  {} M7  No seats by default  ready, no agent has a seat\n",
            item.label
        )),
        "{}",
        fx.text()
    );

    // The default is above 0 and every agent has its own limit of 0.
    let fx = Fx::new();
    for name in ["ann", "bob", "cy", "arch", "dee", "eve"] {
        fx.store
            .queue_set_limit(&QueueCaller::Owner, name, 0)
            .unwrap();
    }
    fx.ready("Every agent at zero");
    let row = one(&fx.json(), "no_taker");
    assert_eq!(row["who"], "owner");
    assert_eq!(row["reason"], "no_seat");
    assert_eq!(strs(&row["next"]), ["another queue limit list"]);
    assert!(fx.text().contains("ready, no agent has a seat\n"));

    // A node with no agents at all.
    let fx = Fx::empty();
    let item = fx
        .store
        .queue_propose(
            &QueueCaller::Owner,
            ProposeItem {
                title: "Nobody is here".into(),
                scope: Some("M7".into()),
                done_when: Some("done".into()),
                ..Default::default()
            },
        )
        .unwrap();
    fx.store
        .queue_admit(&QueueCaller::Owner, &item.label, AdmitItem::default())
        .unwrap();
    let row = one(&fx.json(), "no_taker");
    assert_eq!(row["reason"], "no_seat");
    assert_eq!(strs(&row["next"]), ["another queue limit list"]);

    // The one agent with a seat gave the item up less than an hour ago.
    let fx = Fx::new();
    for name in ["bob", "cy", "arch", "dee", "eve"] {
        fx.store
            .queue_set_limit(&QueueCaller::Owner, name, 0)
            .unwrap();
    }
    let item = fx.ready("Given up");
    assert_eq!(fx.claim("ann").label, item.label);
    fx.store
        .queue_release(
            &QueueCaller::agent("ann"),
            &item.label,
            ReleaseItem::default(),
        )
        .unwrap();
    let row = one(&fx.json(), "no_taker");
    assert_eq!(row["who"], "owner");
    assert_eq!(row["reason"], "skipped");
    assert_eq!(row["agent"], "ann");
    assert_eq!(strs(&row["next"]), ["another queue limit list"]);
    assert!(
        fx.text()
            .contains("ready, ann gave it up less than an hour ago and nobody else has a seat\n"),
        "{}",
        fx.text()
    );
}

#[test]
fn brief_is_one_line_or_nothing() {
    let fx = Fx::new();
    let out = fx.run(&["needs", "--brief"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(text(&out.stdout), "");
    assert_eq!(text(&out.stderr), "");

    fx.everything();
    assert_eq!(
        fx.brief(),
        "needs you: 10 (checks stopped, transports.json refused, 1 decide now, 1 blocked, 1 stuck, 1 unreachable, 1 transport silent, 1 quiet claim, 1 no taker, 1 proposal)\n"
    );
    fx.propose("ann", "One more", QueueKind::Work);
    assert!(fx.brief().ends_with(", 2 proposals)\n"), "{}", fx.brief());

    let v = fx.json_with(&["--brief"]);
    assert_eq!(v["total"], 11);
    assert_eq!(v["counts"]["proposal"], 2);
    assert_eq!(v["rows"], json!([]));
    assert_eq!(v["closing"]["gone_idle"], 1);
}

/// A home as an earlier build left it, one schema behind. Returns the older version.
fn older_home(home: &Path) -> i64 {
    another_core::test_support::older_home(home, 1)
}

fn user_version(home: &Path) -> i64 {
    another_core::test_support::schema_version(&home.join(DB_FILE))
}

#[test]
fn brief_says_when_it_cannot_read() {
    let check = |home: &Path, what: &str| {
        for args in [
            &["needs", "--brief"][..],
            &["--json", "needs", "--brief"][..],
            &["needs", "--brief", "--json"][..],
        ] {
            let out = another(home, args, &[]);
            assert_eq!(out.status.code(), Some(1), "{what} {args:?}");
            assert_eq!(text(&out.stdout), CANNOT_READ, "{what} {args:?}");
        }
    };

    let dir = tempfile::tempdir().unwrap();
    check(&dir.path().join("nothing-here"), "no database");

    let old = dir.path().join("old");
    older_home(&old);
    check(&old, "an older schema");

    // A connection that takes the whole database exclusively blocks even reads.
    let fx = Fx::new();
    fx.propose("ann", "Waiting", QueueKind::Work);
    let Fx { _dir, home, store } = fx;
    drop(store);
    let db = rusqlite::Connection::open(home.join(DB_FILE)).unwrap();
    db.pragma_update(None, "locking_mode", "EXCLUSIVE").unwrap();
    db.execute_batch("BEGIN EXCLUSIVE; UPDATE nodes SET display_name = display_name;")
        .unwrap();
    let started = std::time::Instant::now();
    let out = another(&home, &["needs", "--brief"], &[]);
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(1900),
        "it waited for the lock: {:?}",
        started.elapsed()
    );
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(text(&out.stdout), CANNOT_READ);
    assert!(text(&out.stderr).contains("run another doctor"));
    let out = another(&home, &["--json", "needs", "--brief"], &[]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(text(&out.stdout), CANNOT_READ);
    db.execute_batch("ROLLBACK").unwrap();
    drop(db);
    let out = another(&home, &["needs", "--brief"], &[]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(text(&out.stdout), "needs you: 1 (1 proposal)\n");
}

#[test]
fn the_limit_cuts_rows_not_counts() {
    let fx = Fx::new();
    for n in 0..14 {
        fx.propose("ann", &format!("Proposal {n}"), QueueKind::Work);
    }
    let v = fx.json();
    assert_eq!(v["limit"], 10);
    assert_eq!(v["total"], 14);
    assert_eq!(v["counts"]["proposal"], 14);
    let rows = rows_of(&v, "proposal");
    assert_eq!(rows.len(), 10);
    // Oldest first.
    assert_eq!(rows[0]["item"], "Q1");
    assert_eq!(rows[9]["item"], "Q10");
    let human = fx.text();
    assert!(human.starts_with("14 need the owner ("), "{human}");
    assert!(human.contains("\nProposals (14)\n"), "{human}");
    assert!(
        human.contains("\n  and 4 more: another queue list --state proposed\n"),
        "{human}"
    );
    assert_eq!(human.matches("    next: ").count(), 10);
    assert_eq!(fx.brief(), "needs you: 14 (14 proposals)\n");

    let v = fx.json_with(&["--limit", "3"]);
    assert_eq!(v["limit"], 3);
    assert_eq!(rows_of(&v, "proposal").len(), 3);
    assert_eq!(v["total"], 14);
    let v = fx.json_with(&["--limit", "200"]);
    assert_eq!(rows_of(&v, "proposal").len(), 14);
    assert!(!fx.needs(&["--limit", "200"]).contains(" more: "));

    for bad in ["0", "201", "-1", "many"] {
        let out = fx.run(&["needs", "--limit", bad]);
        assert_eq!(out.status.code(), Some(2), "--limit {bad}");
        assert_eq!(text(&out.stdout), "");
    }
}

/// What a write would change: the event count, the row counts of the tables a hold, a
/// message, an announcement or a session lives in, and every agent's last sign of life.
fn state_of(home: &Path) -> String {
    let conn = rusqlite::Connection::open_with_flags(
        home.join(DB_FILE),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let mut out = String::new();
    for table in [
        "events",
        "holds",
        "messages",
        "deliveries",
        "announcements",
        "transport_sessions",
        "queue_items",
        "queue_history",
        "queue_sessions",
        "wake_targets",
    ] {
        let n: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        out.push_str(&format!("{table} {n}\n"));
    }
    let mut stmt = conn
        .prepare("SELECT agent_id, last_seen_at FROM agents ORDER BY agent_id")
        .unwrap();
    let seen = stmt
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?))
        })
        .unwrap();
    for row in seen {
        out.push_str(&format!("{:?}\n", row.unwrap()));
    }
    out
}

#[test]
fn needs_writes_nothing() {
    let fx = Fx::new();
    fx.everything();
    fx.mail("ann", "cy", "unread", "body");
    let Fx { _dir, home, store } = fx;
    // The last connection closes and checkpoints: the main file now holds everything.
    drop(store);
    let db = home.join(DB_FILE);
    let build = home.join(another_core::ORIGIN_FILE);
    let before = (
        std::fs::read(&db).unwrap(),
        std::fs::read(&build).unwrap(),
        state_of(&home),
    );
    let mut files: Vec<_> = std::fs::read_dir(&home)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    files.sort();

    for args in [
        &["needs"][..],
        &["needs", "--limit", "200"][..],
        &["needs", "--brief"][..],
        &["--json", "needs"][..],
        &["--json", "needs", "--brief"][..],
    ] {
        let out = another(&home, args, &[]);
        assert_eq!(out.status.code(), Some(0), "{args:?}");
        assert!(!out.stdout.is_empty(), "{args:?}");
    }

    assert!(std::fs::read(&db).unwrap() == before.0, "the database file");
    assert!(std::fs::read(&build).unwrap() == before.1, "build.json");
    assert_eq!(state_of(&home), before.2);
    // A reader of a WAL database may leave its empty side files, as doctor does.
    let wal = home.join(format!("{DB_FILE}-wal"));
    assert_eq!(std::fs::metadata(&wal).map_or(0, |m| m.len()), 0);
    let mut after: Vec<_> = std::fs::read_dir(&home)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|name| {
            let name = name.to_string_lossy();
            !name.ends_with("-wal") && !name.ends_with("-shm")
        })
        .collect();
    after.sort();
    files.retain(|name| {
        let name = name.to_string_lossy();
        !name.ends_with("-wal") && !name.ends_with("-shm")
    });
    assert_eq!(after, files, "no file was added to the home");
}

#[test]
fn needs_never_creates_or_migrates_a_home() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nothing-here");
    for args in [&["needs"][..], &["--json", "needs"][..]] {
        let out = another(&missing, args, &[]);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert_eq!(text(&out.stdout), "", "{args:?}");
        let err = text(&out.stderr);
        assert!(err.contains("run `another init` first"), "{err}");
        assert!(err.contains("run another doctor"), "{err}");
    }
    assert!(!missing.exists(), "no home was created");

    let old = dir.path().join("old");
    let version = older_home(&old);
    let out = another(&old, &["needs"], &[]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(text(&out.stdout), "");
    // A release build says a normal open migrates it; a development build that did not
    // make the home says it will not. Either way the schema is named and nothing is done.
    let err = text(&out.stderr);
    assert!(err.contains("schema"), "{err}");
    assert!(err.contains("run another doctor"), "{err}");
    assert_eq!(user_version(&old), version, "nothing was migrated");
    assert!(!old.join(another_core::ORIGIN_FILE).exists());
}

#[cfg(unix)]
#[test]
fn needs_runs_no_program() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fx::new();
    let marker = fx.home.parent().unwrap().join("ran");
    let script = fx.home.parent().unwrap().join("fake-transport");
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho ran >> '{}'\nexit 0\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let program = script.to_string_lossy().into_owned();
    fx.everything();
    // `everything` leaves a refused config; a transport whose every command is the script
    // replaces it.
    transports::write(
        &fx.home,
        &json!({ "version": 1, "transports": { "fake": {
            "wake": [[program, "wake", "{target}", "{message}"]],
            "status": { "argv": [program, "status", "{target}"], "exit_codes": { "0": "idle" } },
            "list": { "argv": [program, "list"] }
        }}})
        .to_string(),
    );
    fx.session("ann", "fake1111", "fake", "t1");
    fx.set_session(
        "ann",
        "fake1111",
        &format!(
            "state = 'blocked', state_since = {now}, attention = 'blocked',
             attention_since = {now}",
            now = now_ms()
        ),
    );
    for args in [
        &["needs"][..],
        &["needs", "--brief"][..],
        &["--json", "needs"][..],
    ] {
        let out = another(&fx.home, args, &[("HERDR_BIN_PATH", program.as_str())]);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
        assert!(!out.stdout.is_empty());
    }
    assert_eq!(fx.json()["counts"]["transport_config"], 0);
    assert_eq!(fx.json()["counts"]["blocked"], 2);
    assert!(!marker.exists(), "a transport program was run");
}

#[test]
fn nothing_unsafe_or_secret_is_printed() {
    let fx = Fx::new();
    // What no write check would let in, put there the way any local process can.
    let unsafe_chars = [
        '\u{1b}', '\u{202e}', '\u{200f}', '\u{061c}', '\u{200b}', '\u{7}',
    ];
    let dirty = |text: &str| -> String {
        let mut out = String::new();
        for c in unsafe_chars {
            out.push(c);
            out.push_str(text);
        }
        out
    };
    let item = fx.propose("ann", "placeholder", QueueKind::Security);
    let held = fx.ready("held");
    assert_eq!(fx.claim("bob").label, held.label);
    fx.store.queue_note("bob", &held.label, "NOTEMARK").unwrap();
    let rejected = fx.propose("ann", "rejected", QueueKind::Work);
    fx.store
        .queue_reject(&QueueCaller::Owner, &rejected.label, "REASONMARK")
        .unwrap();
    fx.mail("ann", "cy", "SUBJECTMARK", "MAILBODYMARK");
    fx.quiet("bob", 90);
    fx.attention(
        "bob",
        "sess-with-a-forty-character-id-TAILMARK1",
        "w1:p1",
        "blocked",
        "blocked",
        3,
    );
    // A stored reason no build knows, and a stored stop, each with an escape sequence
    // that sets the terminal title, a bell, a bidirectional override and a line break.
    let hostile = "evil\u{1b}]0;TITLE\u{7}\u{202e}attn\nSECONDLINE\r$(touch PWNED)";
    fx.attention("dee", "unkn0001", "w1:p2", "idle", "placeholder", 20);
    fx.attention("eve", "stuk0002", "w1:p3", "idle", "stuck", 20);
    let db = fx.db();
    db.execute(
        "UPDATE transport_sessions SET attention = ?1 WHERE session_id = 'unkn0001'",
        [hostile],
    )
    .unwrap();
    db.execute(
        "UPDATE transport_sessions SET wake_stop = ?1, wake_stop_at = ?2,
           last_wake_outcome = ?1, last_wake_at = ?2
         WHERE session_id = 'stuk0002'",
        rusqlite::params![hostile, now_ms() + MIN],
    )
    .unwrap();
    db.execute(
        "UPDATE queue_items SET title = ?1, scope = ?2 WHERE seq = ?3",
        rusqlite::params![dirty("TITLE"), dirty("SC"), item.seq],
    )
    .unwrap();
    db.execute(
        "UPDATE agents SET name = ?1, address = ?1 WHERE agent_id = ?2",
        rusqlite::params![dirty("bob"), fx.id("bob")],
    )
    .unwrap();
    db.execute(
        "UPDATE transport_sessions SET target = ?1, transport = ?2",
        rusqlite::params![dirty("pane"), dirty("herdr")],
    )
    .unwrap();
    db.execute(
        "UPDATE nodes SET display_name = ?1 WHERE is_local = 1",
        [dirty("node")],
    )
    .unwrap();
    // Text that must never be shown at all.
    db.execute(
        "UPDATE queue_items SET body = 'BODYMARK', done_condition = 'DONEMARK'",
        [],
    )
    .unwrap();
    drop(db);

    let human = fx.needs(&["--limit", "200"]);
    let machine = fx.needs(&["--json", "--limit", "200"]);
    let brief = fx.brief();
    assert!(human.contains("TITLE"), "{human:?}");
    assert!(human.contains("pane"), "{human:?}");
    assert!(machine.contains("TITLE"), "{machine}");
    for out in [&human, &machine, &brief] {
        for c in unsafe_chars {
            assert!(
                !out.contains(c),
                "U+{:04X} reached the output: {out:?}",
                c as u32
            );
        }
        for secret in [
            "BODYMARK",
            "DONEMARK",
            "SUBJECTMARK",
            "MAILBODYMARK",
            "NOTEMARK",
            "REASONMARK",
            "TAILMARK1",
            "forty-character",
            "/work/app",
        ] {
            assert!(!out.contains(secret), "{secret} reached the output: {out}");
        }
    }
    // The session id is cut to 4 characters in text and 8 in JSON.
    assert!(human.contains(" sess..  "), "{human:?}");
    let v: Value = serde_json::from_str(&machine).unwrap();
    assert_eq!(one(&v, "blocked")["session"], "sess-wit");

    // The stored reason and the stored stop are shown, cleaned, on one line each.
    let rows = v["rows"].as_array().unwrap();
    let reason_of = |session: &str| {
        rows.iter()
            .find(|r| r["session"] == session)
            .unwrap_or_else(|| panic!("{session} in {v:#}"))["reason"]
            .as_str()
            .unwrap()
            .to_string()
    };
    for session in ["unkn0001", "stuk0002"] {
        let reason = reason_of(session);
        assert!(
            reason.starts_with("evil ]0;TITLE") && reason.contains("SECONDLINE"),
            "{reason:?}"
        );
    }
    assert!(human.contains("  evil ]0;TITLE"), "{human:?}");
    assert!(!human.contains("\nSECONDLINE"), "{human:?}");
    // No decoded JSON value, at any depth, holds a control or direction-changing
    // character: a reader that prints one value prints nothing unsafe.
    fn strings(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::String(s) => out.push(s.clone()),
            Value::Array(a) => a.iter().for_each(|x| strings(x, out)),
            Value::Object(o) => o.iter().for_each(|(k, x)| {
                out.push(k.clone());
                strings(x, out);
            }),
            _ => {}
        }
    }
    let mut decoded = Vec::new();
    strings(&v, &mut decoded);
    strings(&fx.json_with(&["--brief"]), &mut decoded);
    assert!(decoded.len() > 100);
    for value in &decoded {
        assert!(
            !value
                .chars()
                .any(|c| c.is_control() || unsafe_chars.contains(&c)),
            "{value:?}"
        );
    }
    for out in [&human, &brief] {
        assert!(!out.chars().any(|c| c.is_control() && c != '\n'), "{out:?}");
    }
    // Nothing an agent wrote reaches the one-line form.
    assert!(
        brief.starts_with("needs you: ") && !brief.contains("TITLE") && !brief.contains("bob"),
        "{brief:?}"
    );

    // The error path prints nothing from the home either.
    let out = fx.run(&["needs", "--limit", "0"]);
    assert_eq!(out.status.code(), Some(2));
    for c in unsafe_chars {
        assert!(!text(&out.stderr).contains(c));
    }
}
