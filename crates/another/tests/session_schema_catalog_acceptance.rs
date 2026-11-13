#![cfg(not(windows))]

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use rusqlite::{types::Value as SqlValue, Connection};
use serde_json::{json, Value};

// Captured from disposable homes created by mainline 42f44c4228f58b294ea3d257de4cf85ab923c609:
// fresh v8 and v6 (8afda00290259a7963769a99d7edb1d7586dd9a4) upgraded by 42f44c.
// Both were built offline with their locked SQLite 3.50.2. Only schema catalog rows are included;
// this is not a populated historical database or an original published executable.
const HISTORICAL_V8_SCHEMA: &str = r####"[["index","agents_identity","agents","CREATE UNIQUE INDEX agents_identity ON agents(node_id, name, ifnull(project_id, ''))"],["index","deliveries_by_agent","deliveries","CREATE INDEX deliveries_by_agent ON deliveries(agent_id, read_at)"],["index","events_by_kind","events","CREATE INDEX events_by_kind ON events(kind, seq)"],["index","events_by_node","events","CREATE INDEX events_by_node ON events(node_id, seq)"],["index","holds_live","holds","CREATE INDEX holds_live ON holds(project_id, released_at, expires_at)"],["index","messages_by_thread","messages","CREATE INDEX messages_by_thread ON messages(thread_id, created_at)"],["index","oauth_tokens_by_grant","oauth_tokens","CREATE INDEX oauth_tokens_by_grant ON oauth_tokens(grant_id)"],["index","project_repos_by_key","project_repos","CREATE INDEX project_repos_by_key ON project_repos(repo_key)"],["index","requests_by_status","requests","CREATE INDEX requests_by_status ON requests(status, due_at)"],["index","resource_leases_by_agent","resource_leases","CREATE INDEX resource_leases_by_agent ON resource_leases(agent_id, released_at)"],["index","resource_leases_by_resource","resource_leases","CREATE INDEX resource_leases_by_resource ON resource_leases(resource_id, released_at)"],["index","resources_live_identity","resources","CREATE UNIQUE INDEX resources_live_identity ON resources(node_id, kind, name)\n    WHERE released_at IS NULL"],["index","sqlite_autoindex_agents_1","agents",null],["index","sqlite_autoindex_announcements_1","announcements",null],["index","sqlite_autoindex_deliveries_1","deliveries",null],["index","sqlite_autoindex_events_1","events",null],["index","sqlite_autoindex_holds_1","holds",null],["index","sqlite_autoindex_invites_1","invites",null],["index","sqlite_autoindex_messages_1","messages",null],["index","sqlite_autoindex_nodes_1","nodes",null],["index","sqlite_autoindex_oauth_clients_1","oauth_clients",null],["index","sqlite_autoindex_oauth_grants_1","oauth_grants",null],["index","sqlite_autoindex_oauth_requests_1","oauth_requests",null],["index","sqlite_autoindex_oauth_requests_2","oauth_requests",null],["index","sqlite_autoindex_oauth_tokens_1","oauth_tokens",null],["index","sqlite_autoindex_peers_1","peers",null],["index","sqlite_autoindex_project_repos_1","project_repos",null],["index","sqlite_autoindex_projects_1","projects",null],["index","sqlite_autoindex_projects_2","projects",null],["index","sqlite_autoindex_projects_3","projects",null],["index","sqlite_autoindex_requests_1","requests",null],["index","sqlite_autoindex_resource_leases_1","resource_leases",null],["index","sqlite_autoindex_resources_1","resources",null],["index","sqlite_autoindex_tokens_1","tokens",null],["index","sqlite_autoindex_wake_targets_1","wake_targets",null],["table","agents","agents","CREATE TABLE agents (\n    agent_id     TEXT PRIMARY KEY,\n    node_id      TEXT NOT NULL REFERENCES nodes(node_id),\n    project_id   TEXT REFERENCES projects(project_id),\n    name         TEXT NOT NULL,\n    program      TEXT NOT NULL,\n    model        TEXT,\n    description  TEXT,\n    status       TEXT NOT NULL,\n    created_at   INTEGER NOT NULL,\n    updated_at   INTEGER NOT NULL,\n    last_seen_at INTEGER,\n    event_id     TEXT NOT NULL\n, address TEXT)"],["table","announcements","announcements","CREATE TABLE announcements (\n    agent_id     TEXT NOT NULL REFERENCES agents(agent_id),\n    message_id   TEXT NOT NULL REFERENCES messages(message_id),\n    channel      TEXT NOT NULL,\n    announced_at INTEGER NOT NULL,\n    PRIMARY KEY (agent_id, message_id)\n)"],["table","audit_log","audit_log","CREATE TABLE audit_log (\n    audit_id  INTEGER PRIMARY KEY AUTOINCREMENT,\n    node_id   TEXT NOT NULL REFERENCES nodes(node_id),\n    ts        INTEGER NOT NULL,\n    token_id  TEXT,\n    agent     TEXT,\n    operation TEXT NOT NULL,\n    outcome   TEXT NOT NULL,\n    detail    TEXT,\n    remote    TEXT\n)"],["table","deliveries","deliveries","CREATE TABLE deliveries (\n    message_id TEXT NOT NULL REFERENCES messages(message_id),\n    agent_id   TEXT NOT NULL REFERENCES agents(agent_id),\n    node_id    TEXT NOT NULL REFERENCES nodes(node_id),\n    role       TEXT NOT NULL,\n    read_at    INTEGER,\n    acked_at   INTEGER,\n    PRIMARY KEY (message_id, agent_id)\n)"],["table","events","events","CREATE TABLE events (\n    seq      INTEGER PRIMARY KEY AUTOINCREMENT,\n    event_id TEXT NOT NULL UNIQUE,\n    node_id  TEXT NOT NULL REFERENCES nodes(node_id),\n    ts       INTEGER NOT NULL,\n    actor    TEXT NOT NULL,\n    kind     TEXT NOT NULL,\n    payload  TEXT NOT NULL,\n    prev_id  TEXT\n, sig TEXT)"],["table","holds","holds","CREATE TABLE holds (\n    hold_id        TEXT PRIMARY KEY,\n    node_id        TEXT NOT NULL REFERENCES nodes(node_id),\n    project_id     TEXT NOT NULL REFERENCES projects(project_id),\n    agent_id       TEXT NOT NULL REFERENCES agents(agent_id),\n    pattern        TEXT NOT NULL,\n    exclusive      INTEGER NOT NULL,\n    reason         TEXT,\n    ttl_seconds    INTEGER NOT NULL,\n    created_at     INTEGER NOT NULL,\n    renewed_at     INTEGER NOT NULL,\n    expires_at     INTEGER NOT NULL,\n    released_at    INTEGER,\n    release_reason TEXT\n)"],["table","invites","invites","CREATE TABLE invites (\n    invite_id   TEXT PRIMARY KEY,\n    secret_hash TEXT NOT NULL,\n    perms       TEXT NOT NULL,\n    created_at  INTEGER NOT NULL,\n    expires_at  INTEGER NOT NULL,\n    used_at     INTEGER,\n    used_by     TEXT\n)"],["table","messages","messages","CREATE TABLE messages (\n    message_id   TEXT PRIMARY KEY,\n    node_id      TEXT NOT NULL REFERENCES nodes(node_id),\n    thread_id    TEXT NOT NULL,\n    reply_to     TEXT REFERENCES messages(message_id),\n    sender_id    TEXT NOT NULL REFERENCES agents(agent_id),\n    subject      TEXT NOT NULL,\n    body         TEXT NOT NULL,\n    importance   TEXT NOT NULL,\n    ack_required INTEGER NOT NULL DEFAULT 0,\n    created_at   INTEGER NOT NULL,\n    event_id     TEXT NOT NULL\n)"],["table","nodes","nodes","CREATE TABLE nodes (\n    node_id      TEXT PRIMARY KEY,\n    display_name TEXT NOT NULL,\n    role         TEXT NOT NULL DEFAULT 'home',\n    is_local     INTEGER NOT NULL DEFAULT 0,\n    created_at   INTEGER NOT NULL\n, public_key TEXT, secret_key TEXT)"],["table","oauth_clients","oauth_clients","CREATE TABLE oauth_clients (\n    client_id      TEXT PRIMARY KEY,\n    node_id        TEXT NOT NULL REFERENCES nodes(node_id),\n    client_name    TEXT NOT NULL,\n    redirect_uris  TEXT NOT NULL,\n    application_type TEXT NOT NULL,\n    created_at     INTEGER NOT NULL\n)"],["table","oauth_grants","oauth_grants","CREATE TABLE oauth_grants (\n    grant_id      TEXT PRIMARY KEY,\n    node_id       TEXT NOT NULL REFERENCES nodes(node_id),\n    client_id     TEXT NOT NULL,\n    client_name   TEXT NOT NULL,\n    agent_id      TEXT NOT NULL REFERENCES agents(agent_id),\n    project_globs TEXT NOT NULL,\n    label         TEXT,\n    resource      TEXT NOT NULL,\n    scope         TEXT NOT NULL,\n    created_at    INTEGER NOT NULL,\n    revoked_at    INTEGER,\n    last_used_at  INTEGER\n)"],["table","oauth_requests","oauth_requests","CREATE TABLE oauth_requests (\n    request_id            TEXT PRIMARY KEY,\n    node_id               TEXT NOT NULL REFERENCES nodes(node_id),\n    client_id             TEXT NOT NULL,\n    client_name           TEXT NOT NULL,\n    redirect_uri          TEXT NOT NULL,\n    state                 TEXT,\n    scope                 TEXT NOT NULL,\n    resource              TEXT NOT NULL,\n    code_challenge        TEXT NOT NULL,\n    code_challenge_method TEXT NOT NULL,\n    user_code             TEXT NOT NULL UNIQUE,\n    status                TEXT NOT NULL,\n    grant_id              TEXT,\n    redirect_token_hash   TEXT NOT NULL,\n    code_hash             TEXT,\n    created_at            INTEGER NOT NULL,\n    expires_at            INTEGER NOT NULL,\n    code_expires_at       INTEGER\n)"],["table","oauth_tokens","oauth_tokens","CREATE TABLE oauth_tokens (\n    token_id     TEXT PRIMARY KEY,\n    node_id      TEXT NOT NULL REFERENCES nodes(node_id),\n    grant_id     TEXT NOT NULL REFERENCES oauth_grants(grant_id),\n    kind         TEXT NOT NULL,\n    secret_hash  TEXT NOT NULL,\n    resource     TEXT NOT NULL,\n    scope        TEXT NOT NULL,\n    expires_at   INTEGER NOT NULL,\n    revoked_at   INTEGER,\n    replaced_by  TEXT,\n    created_at   INTEGER NOT NULL,\n    last_used_at INTEGER\n)"],["table","peers","peers","CREATE TABLE peers (\n    node_id      TEXT PRIMARY KEY REFERENCES nodes(node_id),\n    role         TEXT NOT NULL,\n    url          TEXT,\n    perms        TEXT NOT NULL,\n    paired_at    INTEGER NOT NULL,\n    push_cursor  INTEGER NOT NULL DEFAULT 0,\n    pull_cursor  INTEGER NOT NULL DEFAULT 0,\n    last_sync_at INTEGER,\n    revoked_at   INTEGER\n)"],["table","project_repos","project_repos","CREATE TABLE project_repos (\n    project_id  TEXT PRIMARY KEY REFERENCES projects(project_id),\n    repo_key    TEXT NOT NULL,\n    repo_prefix TEXT NOT NULL,\n    checked_at  INTEGER NOT NULL\n)"],["table","projects","projects","CREATE TABLE projects (\n    project_id TEXT PRIMARY KEY,\n    node_id    TEXT NOT NULL REFERENCES nodes(node_id),\n    abs_path   TEXT NOT NULL,\n    slug       TEXT NOT NULL,\n    created_at INTEGER NOT NULL,\n    event_id   TEXT NOT NULL,\n    UNIQUE (node_id, abs_path),\n    UNIQUE (node_id, slug)\n)"],["table","requests","requests","CREATE TABLE requests (\n    request_id TEXT PRIMARY KEY,\n    node_id    TEXT NOT NULL REFERENCES nodes(node_id),\n    message_id TEXT NOT NULL REFERENCES messages(message_id),\n    thread_id  TEXT NOT NULL,\n    title      TEXT NOT NULL,\n    created_by TEXT NOT NULL REFERENCES agents(agent_id),\n    owner_id   TEXT REFERENCES agents(agent_id),\n    status     TEXT NOT NULL,\n    due_at     INTEGER,\n    note       TEXT,\n    created_at INTEGER NOT NULL,\n    updated_at INTEGER NOT NULL,\n    claimed_at INTEGER,\n    closed_at  INTEGER\n)"],["table","resource_leases","resource_leases","CREATE TABLE resource_leases (\n    lease_id       TEXT PRIMARY KEY,\n    resource_id    TEXT NOT NULL REFERENCES resources(resource_id),\n    node_id        TEXT NOT NULL REFERENCES nodes(node_id),\n    agent_id       TEXT NOT NULL REFERENCES agents(agent_id),\n    role           TEXT NOT NULL,\n    ttl_seconds    INTEGER NOT NULL,\n    created_at     INTEGER NOT NULL,\n    renewed_at     INTEGER NOT NULL,\n    expires_at     INTEGER NOT NULL,\n    released_at    INTEGER,\n    release_reason TEXT\n)"],["table","resources","resources","CREATE TABLE resources (\n    resource_id    TEXT PRIMARY KEY,\n    node_id        TEXT NOT NULL REFERENCES nodes(node_id),\n    kind           TEXT NOT NULL,\n    name           TEXT NOT NULL,\n    endpoint       TEXT,\n    port           INTEGER,\n    pid            INTEGER,\n    project_id     TEXT REFERENCES projects(project_id),\n    purpose        TEXT,\n    mode           TEXT NOT NULL,\n    metadata       TEXT NOT NULL DEFAULT '{}',\n    created_at     INTEGER NOT NULL,\n    lapsed_at      INTEGER,\n    released_at    INTEGER,\n    release_reason TEXT,\n    event_id       TEXT NOT NULL\n)"],["table","sqlite_sequence","sqlite_sequence","CREATE TABLE sqlite_sequence(name,seq)"],["table","tokens","tokens","CREATE TABLE tokens (\n    token_id      TEXT PRIMARY KEY,\n    node_id       TEXT NOT NULL REFERENCES nodes(node_id),\n    agent_id      TEXT NOT NULL REFERENCES agents(agent_id),\n    label         TEXT,\n    project_globs TEXT NOT NULL,\n    secret_hash   TEXT NOT NULL,\n    created_at    INTEGER NOT NULL,\n    expires_at    INTEGER,\n    revoked_at    INTEGER,\n    last_used_at  INTEGER\n)"],["table","wake_targets","wake_targets","CREATE TABLE wake_targets (\n    agent_id   TEXT PRIMARY KEY REFERENCES agents(agent_id),\n    via        TEXT NOT NULL,\n    target     TEXT NOT NULL,\n    auto       INTEGER NOT NULL DEFAULT 0,\n    updated_at INTEGER NOT NULL\n)"]]"####;

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        let home = temp.path().join("home");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let fixture = Self {
            _temp: temp,
            root,
            home,
        };
        // Hooks never create a home: it exists before any session starts.
        assert!(fixture
            .command()
            .arg("init")
            .output()
            .unwrap()
            .status
            .success());
        let output = fixture.hook("session-start");
        assert!(
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).starts_with("Another: "),
            "fixture start failed: {output:?}"
        );
        fixture
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_another"));
        cmd.env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("ANOTHER_HOME", self.home.join(".another"))
            .env_remove("ANOTHER_AGENT")
            .env_remove("ANOTHER_AGENT_MODE")
            .env_remove("GROK_HOOK_EVENT")
            .env_remove("HERDR_PANE_ID")
            .env_remove("TMUX_PANE")
            .env_remove("CMUX_SURFACE_ID");
        cmd
    }
    fn hook(&self, event: &str) -> Output {
        let mut child = self
            .command()
            .args(["hook", "run", event, "--client", "claude"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let input = json!({"session_id":"catalog-fixture-session", "cwd":self.root,
            "hook_event_name":"PreToolUse", "tool_name":"Edit",
            "tool_input":{"file_path":self.root.join("src/file.rs")}});
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
    fn db(&self) -> Connection {
        Connection::open(self.home.join(".another/another.db")).unwrap()
    }
}

fn data_snapshot(db: &Connection) -> BTreeMap<String, Vec<Vec<SqlValue>>> {
    let mut tables = db.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").unwrap();
    let names = tables
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    names
        .into_iter()
        .map(|name| {
            let sql = format!(
                "SELECT * FROM \"{}\" ORDER BY rowid",
                name.replace('"', "\"\"")
            );
            let mut stmt = db.prepare(&sql).unwrap();
            let columns = stmt.column_count();
            let rows = stmt
                .query_map([], |row| {
                    (0..columns)
                        .map(|i| row.get::<_, SqlValue>(i))
                        .collect::<Result<Vec<_>, _>>()
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            (name, rows)
        })
        .collect()
}

fn assert_refused_without_data_changes(fixture: &Fixture, db: &Connection) {
    let before = data_snapshot(db);
    let version: i64 = db
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    let status = fixture.command().arg("status").output().unwrap();
    assert!(!status.status.success(), "unexpected status success");
    let pre = fixture.hook("pre-edit");
    assert!(pre.status.success(), "hook process failed");
    let reply: Value = serde_json::from_slice(&pre.stdout).expect("expected client denial JSON");
    assert_eq!(reply["hookSpecificOutput"]["permissionDecision"], "deny");
    let after = data_snapshot(db);
    assert!(
        before == after,
        "refusal changed persisted user table contents"
    );
    assert_eq!(
        version,
        db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap()
    );
}

#[test]
fn frozen_historical_schema_and_legitimate_partial_index_are_accepted() {
    let fixture = Fixture::new();
    let db = fixture.db();
    let mut stmt = db
        .prepare("SELECT type,name,tbl_name,sql FROM sqlite_schema ORDER BY type,name")
        .unwrap();
    // v9 only added the admission tables and their indexes. Everything else in a fresh
    // home must still be the frozen v8 catalog, object for object.
    let rows = stmt
        .query_map([], |r| {
            Ok(json!([
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?
            ]))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let (added, kept): (Vec<Value>, Vec<Value>) = rows
        .into_iter()
        .partition(|row| row[2].as_str().unwrap().starts_with("admission"));
    let mut added_tables: Vec<&str> = added
        .iter()
        .filter(|row| row[0] == "table")
        .map(|row| row[1].as_str().unwrap())
        .collect();
    added_tables.sort_unstable();
    assert_eq!(
        added_tables,
        ["admission_holds", "admission_secret", "admissions"]
    );
    let historical: Value = serde_json::from_str(HISTORICAL_V8_SCHEMA).unwrap();
    assert_eq!(
        Value::Array(kept),
        historical,
        "fresh schema diverges from frozen historical v8 catalogs outside the v9 additions"
    );
    let partial: i64 = db.query_row("SELECT partial FROM pragma_index_list('resources') WHERE name='resources_live_identity'", [], |r| r.get(0)).unwrap();
    assert_eq!(
        partial, 1,
        "the shipped resource index is intentionally partial"
    );
    assert!(fixture
        .command()
        .arg("status")
        .output()
        .unwrap()
        .status
        .success());
    let output = fixture.hook("pre-edit");
    assert!(
        output.status.success() && output.stdout.is_empty(),
        "valid partial-index schema should permit the unconflicted guard"
    );
}

#[test]
fn unknown_user_objects_and_damaged_structures_refuse_without_data_loss() {
    for sql in [
        "CREATE TABLE user_private_data(value TEXT); INSERT INTO user_private_data VALUES('retain-me')",
        "CREATE TRIGGER user_event_trigger AFTER INSERT ON events BEGIN SELECT 1; END",
        "DROP INDEX agents_identity",
        "DROP INDEX agents_identity; CREATE INDEX agents_identity ON agents(node_id,name,project_id)",
        "DROP INDEX agents_identity; CREATE UNIQUE INDEX agents_identity ON agents(node_id,name) WHERE project_id IS NOT NULL",
        "DROP INDEX agents_identity; CREATE UNIQUE INDEX agents_identity ON agents(node_id,name,coalesce(project_id,'other'))",
        "ALTER TABLE nodes ADD COLUMN unexpected_column TEXT",
    ] {
        let fixture = Fixture::new();
        let db = fixture.db();
        db.execute_batch(sql).unwrap();
        assert_refused_without_data_changes(&fixture, &db);
    }
}

#[test]
fn catalog_sizes_at_and_above_sentinel_fail_closed() {
    for target in [255_i64, 256, 257] {
        let fixture = Fixture::new();
        let db = fixture.db();
        let count: i64 = db
            .query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))
            .unwrap();
        for i in 0..(target - count) {
            db.execute_batch(&format!("CREATE TABLE extra_catalog_{i}(value TEXT)"))
                .unwrap();
        }
        let actual: i64 = db
            .query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))
            .unwrap();
        assert_eq!(actual, target);
        assert_refused_without_data_changes(&fixture, &db);
    }
}
