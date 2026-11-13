//! SQLite store: connection handling, schema migrations, local node identity and the
//! append-only event log that every mutation writes to first.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::time::{fmt_ts, now_ms};

/// File name of the node database inside the Another home directory.
pub const DB_FILE: &str = "another.db";

/// Source of "now" for the store. Tests use a manual clock to exercise TTLs.
#[derive(Clone)]
pub enum Clock {
    System,
    Manual(Arc<AtomicI64>),
}

impl Clock {
    /// A manual clock starting at `start_ms`; returns the clock and a handle to move it.
    pub fn manual(start_ms: i64) -> (Clock, Arc<AtomicI64>) {
        let handle = Arc::new(AtomicI64::new(start_ms));
        (Clock::Manual(handle.clone()), handle)
    }

    pub fn now(&self) -> i64 {
        match self {
            Clock::System => now_ms(),
            Clock::Manual(t) => t.load(Ordering::SeqCst),
        }
    }
}

/// Resolve the Another home directory: `$ANOTHER_HOME`, else `~/.another`.
pub fn default_home() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("ANOTHER_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error::Invalid("cannot locate home directory; set ANOTHER_HOME".into()))?;
    Ok(PathBuf::from(home).join(".another"))
}

/// Handle to a node database. Cheap to clone; all clones share one connection.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

struct Inner {
    conn: Mutex<Connection>,
    node_id: String,
    key: ed25519_dalek::SigningKey,
    clock: Clock,
    path: Option<PathBuf>,
}

/// Public view of the local node.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct NodeInfo {
    pub node_id: String,
    pub display_name: String,
    pub role: String,
    pub created_at: String,
    /// Ed25519 public key (base64url).
    pub public_key: String,
    /// Short form of the public key, for comparing out of band when pairing.
    pub fingerprint: String,
}

/// One record of the append-only event log.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Event {
    pub seq: i64,
    pub event_id: String,
    pub node_id: String,
    pub ts: String,
    pub actor: String,
    pub kind: String,
    pub payload: Value,
    pub prev_id: Option<String>,
}

impl Store {
    /// Open (creating if needed) the node database inside `home`.
    pub fn open(home: &Path) -> Result<Store> {
        Self::open_with_clock(home, Clock::System)
    }

    pub fn open_with_clock(home: &Path, clock: Clock) -> Result<Store> {
        std::fs::create_dir_all(home)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700))?;
        }
        let path = home.join(DB_FILE);
        let conn = Connection::open(&path)?;
        require_current_or_empty(&conn)?;
        use_wal(&conn)?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(conn, clock, Some(path))
    }

    /// Back up and upgrade an existing home after the operator has fenced all writers.
    /// This cannot verify or maintain the operator's process fence.
    #[cfg(unix)]
    pub fn upgrade_offline(
        home: &Path,
        backup_dir: &Path,
        writer_fence_confirmed: bool,
    ) -> Result<()> {
        use sha2::{Digest, Sha256};
        use std::io::{Read, Write};
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

        if !writer_fence_confirmed {
            return Err(Error::Invalid(
                "operator writer-fence confirmation is required".into(),
            ));
        }
        let source_home = std::fs::canonicalize(home)?;
        let parent = backup_dir
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let name = backup_dir
            .file_name()
            .ok_or_else(|| Error::Invalid("invalid backup directory".into()))?;
        let backup_parent = std::fs::canonicalize(parent)?;
        if backup_parent.starts_with(&source_home) {
            return Err(Error::Invalid(
                "backup directory must be outside the Another home".into(),
            ));
        }
        let backup_dir = backup_parent.join(name);
        let mut source = Connection::open_with_flags(
            source_home.join(DB_FILE),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        )?;
        source.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = source.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let data_version: i64 = source.pragma_query_value(None, "data_version", |r| r.get(0))?;
        if version <= 0 || version >= MIGRATIONS.len() as i64 {
            return Err(Error::Invalid(
                "home is not an older initialized schema".into(),
            ));
        }
        let node: String =
            source.query_row("SELECT node_id FROM nodes WHERE is_local = 1", [], |r| {
                r.get(0)
            })?;
        if node.is_empty() {
            return Err(Error::Invalid("local node identity is missing".into()));
        }
        let key_material: Option<(Option<String>, Option<String>)> = if version >= 4 {
            let pair: (Option<String>, Option<String>) = source.query_row(
                "SELECT public_key, secret_key FROM nodes WHERE is_local = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if pair.0.as_deref().is_none_or(str::is_empty)
                || pair.1.as_deref().is_none_or(str::is_empty)
            {
                return Err(Error::Invalid(
                    "local node key metadata is missing; home was not migrated".into(),
                ));
            }
            let secret =
                crate::crypto::decode_secret(pair.1.as_deref().unwrap()).map_err(|_| {
                    Error::Invalid("local node key is invalid; home was not migrated".into())
                })?;
            if crate::crypto::public_of(&secret) != pair.0.as_deref().unwrap() {
                return Err(Error::Invalid(
                    "local node keys do not match; home was not migrated".into(),
                ));
            }
            Some(pair)
        } else {
            None
        };
        std::fs::DirBuilder::new().mode(0o700).create(&backup_dir)?;
        let backup_path = backup_dir.join(DB_FILE);
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&backup_path)?;
        let mut destination = Connection::open(&backup_path)?;
        {
            let backup = rusqlite::backup::Backup::new(&source, &mut destination)?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                match backup.step(128)? {
                    rusqlite::backup::StepResult::Done => break,
                    _ if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(10))
                    }
                    _ => {
                        return Err(Error::Invalid(
                            "backup timed out; original home was not migrated".into(),
                        ))
                    }
                }
            }
        }
        let check: String =
            destination.pragma_query_value(None, "integrity_check", |r| r.get(0))?;
        let backup_version: i64 =
            destination.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let backup_node: String =
            destination.query_row("SELECT node_id FROM nodes WHERE is_local = 1", [], |r| {
                r.get(0)
            })?;
        if check != "ok" || backup_version != version || backup_node != node {
            return Err(Error::Invalid(
                "backup verification failed; original home was not migrated".into(),
            ));
        }
        if let Some(expected) = key_material {
            let actual: (Option<String>, Option<String>) = destination.query_row(
                "SELECT public_key, secret_key FROM nodes WHERE is_local = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if actual != expected {
                return Err(Error::Invalid(
                    "backup node key verification failed; original home was not migrated".into(),
                ));
            }
        }
        drop(destination);
        let mut file = std::fs::File::open(&backup_path)?;
        file.sync_all()?;
        let mut hash = Sha256::new();
        let mut bytes = [0_u8; 8192];
        loop {
            let n = file.read(&mut bytes)?;
            if n == 0 {
                break;
            }
            hash.update(&bytes[..n]);
        }
        let metadata = serde_json::json!({"schema_version": version, "node_id": node, "database_sha256": format!("{:x}", hash.finalize())});
        let mut record = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(backup_dir.join("backup.json"))?;
        writeln!(record, "{metadata}")?;
        record.sync_all()?;
        std::fs::File::open(&backup_dir)?.sync_all()?;
        migrate_offline(&mut source, version, data_version, &node)
    }

    /// Windows upgrade is blocked until a private backup ACL is implemented and verified.
    #[cfg(not(unix))]
    pub fn upgrade_offline(
        _home: &Path,
        _backup_dir: &Path,
        _writer_fence_confirmed: bool,
    ) -> Result<()> {
        Err(Error::Invalid("offline upgrade is unavailable on this platform; preserve the home and use a verified private backup workflow".into()))
    }

    /// Open an existing database for a hook that runs on every tool call: nothing is created,
    /// nothing is migrated, opening writes nothing, and `busy` bounds every wait for a lock
    /// so a busy database fails fast instead of stalling the agent. Fails when the database
    /// is missing or its schema is not current; an existing old home requires offline upgrade.
    pub fn open_quick(home: &Path, busy: std::time::Duration) -> Result<Store> {
        use rusqlite::OpenFlags;
        let path = home.join(DB_FILE);
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(busy)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version != MIGRATIONS.len() as i64 {
            return Err(Error::Invalid(format!(
                "database schema is v{version}, this build expects v{}; an existing home requires offline upgrade",
                MIGRATIONS.len()
            )));
        }
        let (node_id, secret): (String, Option<String>) = conn.query_row(
            "SELECT node_id, secret_key FROM nodes WHERE is_local = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let secret = secret
            .ok_or_else(|| Error::Invalid("node key missing; a normal open creates it".into()))?;
        Ok(Store {
            inner: Arc::new(Inner {
                conn: Mutex::new(conn),
                node_id,
                key: crate::crypto::decode_secret(&secret)?,
                clock: Clock::System,
                path: Some(path),
            }),
        })
    }

    /// An in-memory store (tests and experiments).
    pub fn open_in_memory(clock: Clock) -> Result<Store> {
        Self::init(Connection::open_in_memory()?, clock, None)
    }

    fn init(mut conn: Connection, clock: Clock, path: Option<PathBuf>) -> Result<Store> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&mut conn)?;
        let (node_id, key) = ensure_local_node(&mut conn, clock.now())?;
        Ok(Store {
            inner: Arc::new(Inner {
                conn: Mutex::new(conn),
                node_id,
                key,
                clock,
                path,
            }),
        })
    }

    pub fn node_id(&self) -> &str {
        &self.inner.node_id
    }

    pub(crate) fn signing_key(&self) -> &ed25519_dalek::SigningKey {
        &self.inner.key
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.inner.path.as_deref()
    }

    pub fn now(&self) -> i64 {
        self.inner.clock.now()
    }

    pub fn node(&self) -> Result<NodeInfo> {
        self.read(|conn| {
            Ok(conn.query_row(
                "SELECT node_id, display_name, role, created_at, public_key FROM nodes
                 WHERE node_id = ?1",
                [self.node_id()],
                |r| {
                    let public_key: String = r.get::<_, Option<String>>(4)?.unwrap_or_default();
                    Ok(NodeInfo {
                        node_id: r.get(0)?,
                        display_name: r.get(1)?,
                        role: r.get(2)?,
                        created_at: fmt_ts(r.get(3)?),
                        fingerprint: crate::crypto::fingerprint(&public_key),
                        public_key,
                    })
                },
            )?)
        })
    }

    /// Most recent events, newest last.
    pub fn events(&self, limit: u32) -> Result<Vec<Event>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT seq, event_id, node_id, ts, actor, kind, payload, prev_id FROM (
                   SELECT * FROM events ORDER BY seq DESC LIMIT ?1
                 ) ORDER BY seq ASC",
            )?;
            let rows = stmt.query_map([limit], |r| {
                let payload: String = r.get(6)?;
                Ok(Event {
                    seq: r.get(0)?,
                    event_id: r.get(1)?,
                    node_id: r.get(2)?,
                    ts: fmt_ts(r.get(3)?),
                    actor: r.get(4)?,
                    kind: r.get(5)?,
                    payload: serde_json::from_str(&payload).unwrap_or(Value::String(payload)),
                    prev_id: r.get(7)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// Run a read-only closure against the connection.
    pub(crate) fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.lock();
        f(&conn)
    }

    /// Run `f` inside an immediate write transaction; commits on `Ok`.
    pub(crate) fn write<T>(&self, f: impl FnOnce(&Tx<'_>) -> Result<T>) -> Result<T> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ctx = Tx {
            tx,
            node_id: &self.inner.node_id,
            now: self.inner.clock.now(),
            key: &self.inner.key,
        };
        let out = f(&ctx)?;
        ctx.tx.commit()?;
        Ok(out)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        // A poisoned lock only means another thread panicked mid-call; the
        // transaction it held was rolled back, so the connection is still usable.
        self.inner
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A write transaction plus the context every mutation needs.
pub(crate) struct Tx<'a> {
    pub tx: rusqlite::Transaction<'a>,
    pub node_id: &'a str,
    pub now: i64,
    pub key: &'a ed25519_dalek::SigningKey,
}

impl std::ops::Deref for Tx<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.tx
    }
}

impl Tx<'_> {
    /// Append an event to the log, chained to this node's previous event. Returns its id.
    pub fn emit(&self, actor: &str, kind: &str, payload: Value) -> Result<String> {
        let prev_id: Option<String> = self
            .query_row(
                "SELECT event_id FROM events WHERE node_id = ?1 ORDER BY seq DESC LIMIT 1",
                [self.node_id],
                |r| r.get(0),
            )
            .optional()?;
        let event_id = new_id();
        let payload = payload.to_string();
        let sig = crate::crypto::sign(
            self.key,
            &crate::crypto::event_message(&event_id, self.node_id, self.now, actor, kind, &payload),
        );
        self.execute(
            "INSERT INTO events (event_id, node_id, ts, actor, kind, payload, prev_id, sig)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                event_id,
                self.node_id,
                self.now,
                actor,
                kind,
                payload,
                prev_id,
                sig
            ],
        )?;
        Ok(event_id)
    }
}

/// A fresh ULID string.
pub fn new_id() -> String {
    ulid::Ulid::new().to_string()
}

const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE nodes (
    node_id      TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    role         TEXT NOT NULL DEFAULT 'home',
    is_local     INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL
);

CREATE TABLE events (
    seq      INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id TEXT NOT NULL UNIQUE,
    node_id  TEXT NOT NULL REFERENCES nodes(node_id),
    ts       INTEGER NOT NULL,
    actor    TEXT NOT NULL,
    kind     TEXT NOT NULL,
    payload  TEXT NOT NULL,
    prev_id  TEXT
);
CREATE INDEX events_by_node ON events(node_id, seq);
CREATE INDEX events_by_kind ON events(kind, seq);

CREATE TABLE projects (
    project_id TEXT PRIMARY KEY,
    node_id    TEXT NOT NULL REFERENCES nodes(node_id),
    abs_path   TEXT NOT NULL,
    slug       TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    event_id   TEXT NOT NULL,
    UNIQUE (node_id, abs_path),
    UNIQUE (node_id, slug)
);

CREATE TABLE agents (
    agent_id     TEXT PRIMARY KEY,
    node_id      TEXT NOT NULL REFERENCES nodes(node_id),
    project_id   TEXT REFERENCES projects(project_id),
    name         TEXT NOT NULL,
    program      TEXT NOT NULL,
    model        TEXT,
    description  TEXT,
    status       TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    last_seen_at INTEGER,
    event_id     TEXT NOT NULL
);
CREATE UNIQUE INDEX agents_identity ON agents(node_id, name, ifnull(project_id, ''));

CREATE TABLE messages (
    message_id   TEXT PRIMARY KEY,
    node_id      TEXT NOT NULL REFERENCES nodes(node_id),
    thread_id    TEXT NOT NULL,
    reply_to     TEXT REFERENCES messages(message_id),
    sender_id    TEXT NOT NULL REFERENCES agents(agent_id),
    subject      TEXT NOT NULL,
    body         TEXT NOT NULL,
    importance   TEXT NOT NULL,
    ack_required INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL,
    event_id     TEXT NOT NULL
);
CREATE INDEX messages_by_thread ON messages(thread_id, created_at);

CREATE TABLE deliveries (
    message_id TEXT NOT NULL REFERENCES messages(message_id),
    agent_id   TEXT NOT NULL REFERENCES agents(agent_id),
    node_id    TEXT NOT NULL REFERENCES nodes(node_id),
    role       TEXT NOT NULL,
    read_at    INTEGER,
    acked_at   INTEGER,
    PRIMARY KEY (message_id, agent_id)
);
CREATE INDEX deliveries_by_agent ON deliveries(agent_id, read_at);

CREATE TABLE requests (
    request_id TEXT PRIMARY KEY,
    node_id    TEXT NOT NULL REFERENCES nodes(node_id),
    message_id TEXT NOT NULL REFERENCES messages(message_id),
    thread_id  TEXT NOT NULL,
    title      TEXT NOT NULL,
    created_by TEXT NOT NULL REFERENCES agents(agent_id),
    owner_id   TEXT REFERENCES agents(agent_id),
    status     TEXT NOT NULL,
    due_at     INTEGER,
    note       TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    claimed_at INTEGER,
    closed_at  INTEGER
);
CREATE INDEX requests_by_status ON requests(status, due_at);

CREATE TABLE holds (
    hold_id        TEXT PRIMARY KEY,
    node_id        TEXT NOT NULL REFERENCES nodes(node_id),
    project_id     TEXT NOT NULL REFERENCES projects(project_id),
    agent_id       TEXT NOT NULL REFERENCES agents(agent_id),
    pattern        TEXT NOT NULL,
    exclusive      INTEGER NOT NULL,
    reason         TEXT,
    ttl_seconds    INTEGER NOT NULL,
    created_at     INTEGER NOT NULL,
    renewed_at     INTEGER NOT NULL,
    expires_at     INTEGER NOT NULL,
    released_at    INTEGER,
    release_reason TEXT
);
CREATE INDEX holds_live ON holds(project_id, released_at, expires_at);
"#,
    r#"
CREATE TABLE resources (
    resource_id    TEXT PRIMARY KEY,
    node_id        TEXT NOT NULL REFERENCES nodes(node_id),
    kind           TEXT NOT NULL,
    name           TEXT NOT NULL,
    endpoint       TEXT,
    port           INTEGER,
    pid            INTEGER,
    project_id     TEXT REFERENCES projects(project_id),
    purpose        TEXT,
    mode           TEXT NOT NULL,
    metadata       TEXT NOT NULL DEFAULT '{}',
    created_at     INTEGER NOT NULL,
    lapsed_at      INTEGER,
    released_at    INTEGER,
    release_reason TEXT,
    event_id       TEXT NOT NULL
);
CREATE UNIQUE INDEX resources_live_identity ON resources(node_id, kind, name)
    WHERE released_at IS NULL;

CREATE TABLE resource_leases (
    lease_id       TEXT PRIMARY KEY,
    resource_id    TEXT NOT NULL REFERENCES resources(resource_id),
    node_id        TEXT NOT NULL REFERENCES nodes(node_id),
    agent_id       TEXT NOT NULL REFERENCES agents(agent_id),
    role           TEXT NOT NULL,
    ttl_seconds    INTEGER NOT NULL,
    created_at     INTEGER NOT NULL,
    renewed_at     INTEGER NOT NULL,
    expires_at     INTEGER NOT NULL,
    released_at    INTEGER,
    release_reason TEXT
);
CREATE INDEX resource_leases_by_resource ON resource_leases(resource_id, released_at);
CREATE INDEX resource_leases_by_agent ON resource_leases(agent_id, released_at);
"#,
    r#"
CREATE TABLE tokens (
    token_id      TEXT PRIMARY KEY,
    node_id       TEXT NOT NULL REFERENCES nodes(node_id),
    agent_id      TEXT NOT NULL REFERENCES agents(agent_id),
    label         TEXT,
    project_globs TEXT NOT NULL,
    secret_hash   TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    expires_at    INTEGER,
    revoked_at    INTEGER,
    last_used_at  INTEGER
);

CREATE TABLE audit_log (
    audit_id  INTEGER PRIMARY KEY AUTOINCREMENT,
    node_id   TEXT NOT NULL REFERENCES nodes(node_id),
    ts        INTEGER NOT NULL,
    token_id  TEXT,
    agent     TEXT,
    operation TEXT NOT NULL,
    outcome   TEXT NOT NULL,
    detail    TEXT,
    remote    TEXT
);
"#,
    r#"
ALTER TABLE nodes ADD COLUMN public_key TEXT;
ALTER TABLE nodes ADD COLUMN secret_key TEXT;
ALTER TABLE events ADD COLUMN sig TEXT;
ALTER TABLE agents ADD COLUMN address TEXT;
UPDATE agents SET address = name || coalesce(
    '@' || (SELECT slug FROM projects p WHERE p.project_id = agents.project_id), '');

CREATE TABLE peers (
    node_id      TEXT PRIMARY KEY REFERENCES nodes(node_id),
    role         TEXT NOT NULL,
    url          TEXT,
    perms        TEXT NOT NULL,
    paired_at    INTEGER NOT NULL,
    push_cursor  INTEGER NOT NULL DEFAULT 0,
    pull_cursor  INTEGER NOT NULL DEFAULT 0,
    last_sync_at INTEGER,
    revoked_at   INTEGER
);

CREATE TABLE invites (
    invite_id   TEXT PRIMARY KEY,
    secret_hash TEXT NOT NULL,
    perms       TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL,
    used_at     INTEGER,
    used_by     TEXT
);
"#,
    r#"
CREATE TABLE wake_targets (
    agent_id   TEXT PRIMARY KEY REFERENCES agents(agent_id),
    via        TEXT NOT NULL,
    target     TEXT NOT NULL,
    auto       INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL
);
"#,
    r#"
CREATE TABLE oauth_clients (
    client_id      TEXT PRIMARY KEY,
    node_id        TEXT NOT NULL REFERENCES nodes(node_id),
    client_name    TEXT NOT NULL,
    redirect_uris  TEXT NOT NULL,
    application_type TEXT NOT NULL,
    created_at     INTEGER NOT NULL
);

CREATE TABLE oauth_grants (
    grant_id      TEXT PRIMARY KEY,
    node_id       TEXT NOT NULL REFERENCES nodes(node_id),
    client_id     TEXT NOT NULL,
    client_name   TEXT NOT NULL,
    agent_id      TEXT NOT NULL REFERENCES agents(agent_id),
    project_globs TEXT NOT NULL,
    label         TEXT,
    resource      TEXT NOT NULL,
    scope         TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    revoked_at    INTEGER,
    last_used_at  INTEGER
);

CREATE TABLE oauth_requests (
    request_id            TEXT PRIMARY KEY,
    node_id               TEXT NOT NULL REFERENCES nodes(node_id),
    client_id             TEXT NOT NULL,
    client_name           TEXT NOT NULL,
    redirect_uri          TEXT NOT NULL,
    state                 TEXT,
    scope                 TEXT NOT NULL,
    resource              TEXT NOT NULL,
    code_challenge        TEXT NOT NULL,
    code_challenge_method TEXT NOT NULL,
    user_code             TEXT NOT NULL UNIQUE,
    status                TEXT NOT NULL,
    grant_id              TEXT,
    redirect_token_hash   TEXT NOT NULL,
    code_hash             TEXT,
    created_at            INTEGER NOT NULL,
    expires_at            INTEGER NOT NULL,
    code_expires_at       INTEGER
);

CREATE TABLE oauth_tokens (
    token_id     TEXT PRIMARY KEY,
    node_id      TEXT NOT NULL REFERENCES nodes(node_id),
    grant_id     TEXT NOT NULL REFERENCES oauth_grants(grant_id),
    kind         TEXT NOT NULL,
    secret_hash  TEXT NOT NULL,
    resource     TEXT NOT NULL,
    scope        TEXT NOT NULL,
    expires_at   INTEGER NOT NULL,
    revoked_at   INTEGER,
    replaced_by  TEXT,
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER
);
CREATE INDEX oauth_tokens_by_grant ON oauth_tokens(grant_id);
"#,
    // Node-local cache of which git repository each local project belongs to (see repo.rs).
    // Derived from files on this machine, so it is not an event and never syncs.
    // An empty repo_key means "checked: not in a git repository".
    r#"
CREATE TABLE project_repos (
    project_id  TEXT PRIMARY KEY REFERENCES projects(project_id),
    repo_key    TEXT NOT NULL,
    repo_prefix TEXT NOT NULL,
    checked_at  INTEGER NOT NULL
);
CREATE INDEX project_repos_by_key ON project_repos(repo_key);
"#,
    // Which messages have been announced to which agent (prompt or mid-task notice, or a
    // daemon wake). One row per (agent, message) is the claim that makes "announced once"
    // hold across concurrent hooks and the daemon. Node-local; the evidence is the
    // `push.notified` / `push.woke` event written in the same transaction.
    r#"
CREATE TABLE announcements (
    agent_id     TEXT NOT NULL REFERENCES agents(agent_id),
    message_id   TEXT NOT NULL REFERENCES messages(message_id),
    channel      TEXT NOT NULL,
    announced_at INTEGER NOT NULL,
    PRIMARY KEY (agent_id, message_id)
);
"#,
    r#"
CREATE TABLE session_bindings (
    digest TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL UNIQUE REFERENCES agents(agent_id),
    project_id TEXT NOT NULL REFERENCES projects(project_id),
    client TEXT NOT NULL,
    base_name TEXT NOT NULL
);
"#,
];

/// Switch to WAL. While several connections convert a new database at once, SQLite answers
/// busy without calling the busy handler, so retry for up to the usual busy timeout.
fn use_wal(conn: &Connection) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match conn.pragma_update(None, "journal_mode", "WAL") {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::DatabaseBusy
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            other => return Ok(other?),
        }
    }
}

fn require_current_or_empty(conn: &Connection) -> Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version > MIGRATIONS.len() as i64 {
        return Err(Error::Invalid(format!(
            "database schema v{version} is newer than this build"
        )));
    }
    if version > 0 && version < MIGRATIONS.len() as i64 {
        return Err(Error::Invalid("existing home requires offline upgrade: fence all writers, then run `another upgrade --backup-dir <new-directory> --confirm-writer-fence`".into()));
    }
    if version == 0 {
        let objects: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )?;
        if objects != 0 {
            return Err(Error::Invalid(
                "unversioned nonempty database; preserve it and inspect before upgrade".into(),
            ));
        }
    }
    Ok(())
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    require_current_or_empty(&tx)?;
    let current: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if current == 0 {
        for sql in MIGRATIONS {
            tx.execute_batch(sql)?;
        }
        tx.pragma_update(None, "user_version", MIGRATIONS.len() as i64)?;
    }
    tx.commit()?;
    Ok(())
}

#[cfg(unix)]
fn migrate_offline(
    conn: &mut Connection,
    version: i64,
    data_version: i64,
    node: &str,
) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let actual: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let data: i64 = tx.pragma_query_value(None, "data_version", |r| r.get(0))?;
    let actual_node: String =
        tx.query_row("SELECT node_id FROM nodes WHERE is_local = 1", [], |r| {
            r.get(0)
        })?;
    if actual != version || data != data_version || actual_node != node {
        return Err(Error::Invalid(
            "source changed during backup; preserve both homes and repeat after fencing writers"
                .into(),
        ));
    }
    for sql in &MIGRATIONS[version as usize..] {
        tx.execute_batch(sql)?;
    }
    tx.pragma_update(None, "user_version", MIGRATIONS.len() as i64)?;
    tx.commit().map_err(|e| Error::Invalid(format!("upgrade commit outcome uncertain ({e}); keep the writer fence and preserve both source and backup")))?;
    Ok(())
}

fn ensure_local_node(
    conn: &mut Connection,
    now: i64,
) -> Result<(String, ed25519_dalek::SigningKey)> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let existing: Option<(String, Option<String>)> = tx
        .query_row(
            "SELECT node_id, secret_key FROM nodes WHERE is_local = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((node_id, secret)) = existing {
        let key = match secret {
            Some(s) => crate::crypto::decode_secret(&s)?,
            None => {
                // Databases created before node keys existed: create one now and sign the
                // events this node already authored.
                let key = crate::crypto::generate_key()?;
                tx.execute(
                    "UPDATE nodes SET secret_key = ?2, public_key = ?3 WHERE node_id = ?1",
                    params![
                        node_id,
                        crate::crypto::b64(key.as_bytes()),
                        crate::crypto::public_of(&key)
                    ],
                )?;
                key
            }
        };
        sign_unsigned(&tx, &node_id, &key)?;
        tx.commit()?;
        return Ok((node_id, key));
    }
    let node_id = new_id();
    let key = crate::crypto::generate_key()?;
    let display_name = std::env::var("ANOTHER_NODE_NAME").unwrap_or_else(|_| "local".to_string());
    tx.execute(
        "INSERT INTO nodes (node_id, display_name, role, is_local, created_at, public_key, secret_key)
         VALUES (?1, ?2, 'home', 1, ?3, ?4, ?5)",
        params![
            node_id,
            display_name,
            now,
            crate::crypto::public_of(&key),
            crate::crypto::b64(key.as_bytes())
        ],
    )?;
    {
        let ctx = Tx {
            tx,
            node_id: &node_id,
            now,
            key: &key,
        };
        ctx.emit(
            "system",
            "node.created",
            serde_json::json!({
                "node_id": node_id,
                "display_name": display_name,
                "role": "home",
                "public_key": crate::crypto::public_of(&key),
            }),
        )?;
        ctx.tx.commit()?;
    }
    Ok((node_id, key))
}

fn sign_unsigned(conn: &Connection, node_id: &str, key: &ed25519_dalek::SigningKey) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT event_id, ts, actor, kind, payload FROM events WHERE node_id = ?1 AND sig IS NULL",
    )?;
    let rows = stmt
        .query_map([node_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (event_id, ts, actor, kind, payload) in rows {
        let sig = crate::crypto::sign(
            key,
            &crate::crypto::event_message(&event_id, node_id, ts, &actor, &kind, &payload),
        );
        conn.execute(
            "UPDATE events SET sig = ?2 WHERE event_id = ?1",
            params![event_id, sig],
        )?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod upgrade_tests {
    use super::*;

    #[test]
    fn intervening_write_refuses_upgrade_without_discarding_it() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        drop(Store::open(&home).unwrap());
        let db = home.join(DB_FILE);
        let source = Connection::open(&db).unwrap();
        source
            .execute_batch("DROP TABLE session_bindings; PRAGMA user_version=8;")
            .unwrap();
        let version: i64 = source
            .pragma_query_value(None, "data_version", |r| r.get(0))
            .unwrap();
        let node: String = source
            .query_row("SELECT node_id FROM nodes WHERE is_local=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        let writer = Connection::open(&db).unwrap();
        writer
            .execute_batch(
                "CREATE TABLE intervening(v TEXT); INSERT INTO intervening VALUES ('newer');",
            )
            .unwrap();
        let mut source = source;
        let result = migrate_offline(&mut source, 8, version, &node);
        assert!(result.is_err());
        assert_eq!(
            source
                .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            8
        );
        assert_eq!(
            source
                .query_row("SELECT v FROM intervening", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "newer"
        );
    }
}
