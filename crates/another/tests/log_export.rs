//! `another log export`: JSON Lines, oldest first, a cursor that continues without gaps
//! or repeats, filters, the audit log, and an export that writes nothing.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use another_core::{AuditRecord, Program, RegisterAgent, SendMessage, Store, DB_FILE};
use serde_json::Value;

struct Env {
    _dir: tempfile::TempDir,
    home: PathBuf,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let store = Store::open(&home).unwrap();
        for (name, program) in [("alice", Program::Claude), ("bob", Program::Codex)] {
            store
                .register_agent(RegisterAgent {
                    name: name.into(),
                    program,
                    project: None,
                    model: None,
                    description: None,
                })
                .unwrap();
        }
        Env { _dir: dir, home }
    }

    fn store(&self) -> Store {
        Store::open(&self.home).unwrap()
    }

    fn send(&self, n: usize) {
        let store = self.store();
        for i in 0..n {
            store
                .send_message(SendMessage {
                    from: "alice".into(),
                    to: vec!["bob".into()],
                    subject: format!("note {i}"),
                    body: "body".into(),
                    ..SendMessage::default()
                })
                .unwrap();
        }
    }

    fn run_in(home: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_another"))
            .args(args)
            .env("ANOTHER_HOME", home)
            .env_remove("ANOTHER_AGENT")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// `another log export ARGS`: the lines and the stderr text. Must succeed.
    fn export(&self, args: &[&str]) -> (Vec<Value>, String) {
        let mut all = vec!["log", "export"];
        all.extend_from_slice(args);
        let out = Env::run_in(&self.home, &all);
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(out.status.success(), "export {args:?} failed: {err}");
        let lines = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).expect("every stdout line is JSON"))
            .collect();
        (lines, err)
    }

    fn dump(&self) -> Vec<String> {
        let db = rusqlite::Connection::open(self.home.join(DB_FILE)).unwrap();
        let tables: Vec<String> = db
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        tables
            .into_iter()
            .map(|t| {
                let mut stmt = db.prepare(&format!("SELECT * FROM \"{t}\"")).unwrap();
                let n = stmt.column_count();
                let mut rows: Vec<String> = stmt
                    .query_map([], |r| {
                        (0..n)
                            .map(|i| {
                                r.get::<_, rusqlite::types::Value>(i)
                                    .map(|v| format!("{v:?}"))
                            })
                            .collect::<rusqlite::Result<Vec<_>>>()
                            .map(|c| c.join("|"))
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap();
                rows.sort();
                format!("{t}: {}", rows.join("\n"))
            })
            .collect()
    }
}

fn seqs(lines: &[Value]) -> Vec<i64> {
    lines.iter().map(|l| l["seq"].as_i64().unwrap()).collect()
}

/// The event id named by the stderr cursor line.
fn cursor(err: &str) -> String {
    err.lines()
        .rev()
        .find_map(|l| l.split("last event_id ").nth(1))
        .expect("a cursor line")
        .trim()
        .to_string()
}

#[test]
fn the_cursor_continues_without_gaps_or_repeats() {
    let env = Env::new();
    // Past one page (1,000 rows) so paging is exercised.
    env.send(1_200);
    let before = env.dump();
    let (all, err) = env.export(&[]);
    assert_eq!(before, env.dump(), "the export wrote to the database");
    let s = seqs(&all);
    assert!(s.len() > 1_200, "{}", s.len());
    assert!(s.windows(2).all(|w| w[0] < w[1]), "not oldest first");
    assert_eq!(
        cursor(&err),
        all.last().unwrap()["event_id"].as_str().unwrap()
    );

    // Stop part-way, then continue from the cursor: together they are the whole log.
    let (first, err) = env.export(&["--limit", "700"]);
    assert_eq!(first.len(), 700);
    let (rest, _) = env.export(&["--since", &cursor(&err)]);
    let mut joined = seqs(&first);
    joined.extend(seqs(&rest));
    assert_eq!(joined, s);

    // New events after an export are exactly what the next export returns.
    let (_, err) = env.export(&[]);
    env.send(3);
    let (next, _) = env.export(&["--since", &cursor(&err)]);
    assert_eq!(next.len(), 3);
    assert!(next.iter().all(|l| l["kind"] == "message.sent"));
}

#[test]
fn lines_have_the_documented_shape_and_filters_apply() {
    let env = Env::new();
    env.send(2);
    let (lines, _) = env.export(&["--kind", "message.sent"]);
    assert_eq!(lines.len(), 2);
    let l = &lines[0];
    for key in [
        "seq", "event_id", "node_id", "ts", "actor", "kind", "payload", "prev_id",
    ] {
        assert!(l.get(key).is_some(), "missing {key}: {l}");
    }
    assert!(l["payload"].is_object(), "{l}");
    assert!(l["ts"].as_str().unwrap().ends_with('Z'), "{l}");

    let (none, _) = env.export(&[
        "--since",
        "2000-01-01T00:00:00Z",
        "--until",
        "2000-01-02T00:00:00Z",
    ]);
    assert!(none.is_empty());
    let (recent, _) = env.export(&["--since", "1h", "--kind", "message.sent,agent.registered"]);
    assert_eq!(recent.len(), 4);
}

#[test]
fn a_payload_that_does_not_parse_is_kept_as_text() {
    let env = Env::new();
    let db = rusqlite::Connection::open(env.home.join(DB_FILE)).unwrap();
    let node: String = db
        .query_row("SELECT node_id FROM nodes WHERE is_local = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    db.execute(
        "INSERT INTO events (event_id, node_id, ts, actor, kind, payload)
         VALUES ('01ZZZZZZZZZZZZZZZZZZZZZZZZ', ?1, 1, 'test', 'test.raw', 'not json')",
        [&node],
    )
    .unwrap();
    // A peer's event is left out unless asked for.
    db.execute(
        "INSERT INTO nodes (node_id, display_name, created_at) VALUES ('PEER', 'peer', 1)",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO events (event_id, node_id, ts, actor, kind, payload)
         VALUES ('01YYYYYYYYYYYYYYYYYYYYYYYY', 'PEER', 2, 'p', 'peer.thing', '{}')",
        [],
    )
    .unwrap();
    drop(db);
    let (raw, _) = env.export(&["--kind", "test.raw"]);
    assert_eq!(raw[0]["payload"], "not json");
    assert_eq!(raw[0]["payload_raw"], true);
    let (local, _) = env.export(&["--kind", "peer.thing"]);
    assert!(local.is_empty());
    let (all, _) = env.export(&["--kind", "peer.thing", "--node", "all"]);
    assert_eq!(all.len(), 1);
}

#[test]
fn the_audit_log_exports_by_audit_id() {
    let env = Env::new();
    let store = env.store();
    for op in ["GET /activity", "tools/call send_message", "GET /holds"] {
        store
            .record_audit(AuditRecord {
                token_id: Some("tok".into()),
                agent: Some("alice".into()),
                operation: op.into(),
                outcome: "ok".into(),
                detail: None,
                remote: Some("127.0.0.1".into()),
            })
            .unwrap();
    }
    let (rows, err) = env.export(&["--audit"]);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1]["operation"], "tools/call send_message");
    assert!(err.contains("last audit id 3"), "{err}");
    let (rest, _) = env.export(&["--audit", "--since", "1"]);
    assert_eq!(rest.len(), 2);
    // An event id means nothing in the audit log.
    let out = Env::run_in(
        &env.home,
        &[
            "log",
            "export",
            "--audit",
            "--since",
            "01ZZZZZZZZZZZZZZZZZZZZZZZZ",
        ],
    );
    assert!(!out.status.success());
}

#[test]
fn it_never_creates_or_migrates_a_home() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("none");
    let out = Env::run_in(&missing, &["log", "export"]);
    assert!(!out.status.success());
    assert!(
        !missing.join(DB_FILE).exists(),
        "the export created a database"
    );

    let env = Env::new();
    let db = rusqlite::Connection::open(env.home.join(DB_FILE)).unwrap();
    let v: i64 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    db.pragma_update(None, "user_version", v - 1).unwrap();
    drop(db);
    let out = Env::run_in(&env.home, &["log", "export"]);
    assert!(
        !out.status.success(),
        "an old schema was exported or migrated"
    );
    let db = rusqlite::Connection::open(env.home.join(DB_FILE)).unwrap();
    let after: i64 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(after, v - 1, "the export migrated the database");
}

/// FINDINGS-164 F2: a peer's message body is redacted (D41) unless asked for; this node's
/// own bodies are kept.
#[test]
fn peers_message_bodies_are_redacted_unless_asked_for() {
    let env = Env::new();
    env.send(1);
    let db = rusqlite::Connection::open(env.home.join(DB_FILE)).unwrap();
    db.execute(
        "INSERT INTO nodes (node_id, display_name, created_at) VALUES ('PEER', 'peer', 1)",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO events (event_id, node_id, ts, actor, kind, payload)
         VALUES ('01XXXXXXXXXXXXXXXXXXXXXXXX', 'PEER', 3, 'p', 'message.sent',
                 '{\"subject\":\"from afar\",\"body\":\"secret words\"}')",
        [],
    )
    .unwrap();
    drop(db);
    let (lines, _) = env.export(&["--kind", "message.sent", "--node", "all"]);
    let peer = lines.iter().find(|l| l["node_id"] == "PEER").unwrap();
    assert_eq!(peer["payload"]["body"], Value::Null, "{peer}");
    assert_eq!(peer["payload"]["body_redacted_bytes"], 12, "{peer}");
    assert_eq!(peer["payload"]["subject"], "from afar");
    let own = lines.iter().find(|l| l["node_id"] != "PEER").unwrap();
    assert_eq!(own["payload"]["body"], "body", "{own}");

    let (lines, _) = env.export(&[
        "--kind",
        "message.sent",
        "--node",
        "all",
        "--include-foreign-bodies",
    ]);
    let peer = lines.iter().find(|l| l["node_id"] == "PEER").unwrap();
    assert_eq!(peer["payload"]["body"], "secret words");
}

/// FINDINGS-164 note: a kind the log has never held is named, not silently empty.
#[test]
fn an_unknown_kind_is_named_on_stderr() {
    let env = Env::new();
    let (lines, err) = env.export(&["--kind", "messsage.sent"]);
    assert!(lines.is_empty());
    assert!(err.contains("no event of kind messsage.sent"), "{err}");
}

/// FINDINGS-164 F1: the order is log order (`seq`), even when a later event has an earlier
/// time, so the cursor stays gap-free.
#[test]
fn the_order_is_log_order_even_when_time_goes_backwards() {
    let env = Env::new();
    env.send(1);
    let db = rusqlite::Connection::open(env.home.join(DB_FILE)).unwrap();
    let node: String = db
        .query_row("SELECT node_id FROM nodes WHERE is_local = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    db.execute(
        "INSERT INTO events (event_id, node_id, ts, actor, kind, payload)
         VALUES ('01WWWWWWWWWWWWWWWWWWWWWWWW', ?1, 1, 'test', 'test.backwards', '{}')",
        [&node],
    )
    .unwrap();
    drop(db);
    let (lines, _) = env.export(&[]);
    let s = seqs(&lines);
    assert!(s.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(lines.last().unwrap()["kind"], "test.backwards");
}
