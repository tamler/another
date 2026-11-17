//! SQLite store: connection handling, schema migrations, local node identity and the
//! append-only event log that every mutation writes to first.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::time::{fmt_ts, now_ms};

/// File name of the node database inside the Another home directory.
pub const DB_FILE: &str = "another.db";

/// File in the home that records which build created the database and which last migrated it.
pub const ORIGIN_FILE: &str = "build.json";

/// Directory in the home that holds the copy of the database taken before each migration.
pub const BACKUP_DIR: &str = "backups";

static ALLOW_DEV_MIGRATE: AtomicBool = AtomicBool::new(false);

/// Let this process, when it is a development build, migrate a home that a development build
/// did not create. The CLI calls this for `--dev-migrate`.
pub fn allow_dev_migrate() {
    ALLOW_DEV_MIGRATE.store(true, Ordering::SeqCst);
}

/// The build that is opening a home, as far as upgrade safety cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Build {
    /// Compiled with debug assertions: `cargo build`, `cargo run` and `cargo test`. The
    /// release profile (`cargo install`, the release archives) is not a development build.
    pub development: bool,
    /// The explicit permission to migrate a home that belongs to another build.
    pub allow_dev_migrate: bool,
    /// The running executable, with symlinks resolved. `None` when the system cannot say.
    pub binary: Option<PathBuf>,
}

impl Build {
    /// This binary.
    pub fn current() -> Build {
        static BINARY: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
        Build {
            development: cfg!(debug_assertions),
            allow_dev_migrate: ALLOW_DEV_MIGRATE.load(Ordering::SeqCst),
            binary: BINARY
                .get_or_init(|| std::env::current_exe().and_then(std::fs::canonicalize).ok())
                .clone(),
        }
    }

    fn binary_text(&self) -> Option<String> {
        self.binary.as_ref().map(|path| path.display().to_string())
    }

    fn stamp(&self, schema: i64, now: i64) -> BuildStamp {
        BuildStamp {
            version: env!("CARGO_PKG_VERSION").to_string(),
            build: if self.development {
                DEVELOPMENT.to_string()
            } else {
                "release".to_string()
            },
            schema,
            at: fmt_ts(now),
            binary: self.binary_text(),
        }
    }
}

const DEVELOPMENT: &str = "development";

/// How many backups a successful migration leaves in [`BACKUP_DIR`].
const BACKUPS_KEPT: usize = 5;

/// One build's mark in [`ORIGIN_FILE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildStamp {
    /// The build's package version.
    pub version: String,
    /// `release` or `development`.
    pub build: String,
    /// The schema version the build left the database at.
    pub schema: i64,
    pub at: String,
    /// The build's executable, with symlinks resolved. Stamps from before it was recorded
    /// have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
}

/// The table check of one schema, remembered so later opens can skip it: the schema version,
/// SQLite's schema cookie at the time (`PRAGMA schema_version`, which every schema change
/// moves) and the digest the checking build holds for that version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verified {
    pub schema: i64,
    pub cookie: i64,
    pub digest: String,
}

/// The contents of [`ORIGIN_FILE`]: which build created the database, which release build
/// first took on a home that had no binary on record, and which build last migrated it. A
/// home from before the file existed has none of them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomeOrigin {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<BuildStamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopted: Option<BuildStamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migrated: Option<BuildStamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<Verified>,
}

impl HomeOrigin {
    /// What `home` records. A missing or unreadable file reads as no record, which is the
    /// careful answer: a development build then refuses to migrate the home.
    fn read(home: &Path) -> HomeOrigin {
        std::fs::read(home.join(ORIGIN_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn write(&self, home: &Path) -> Result<()> {
        let path = home.join(ORIGIN_FILE);
        let staged = home.join(format!("{ORIGIN_FILE}.{}.tmp", std::process::id()));
        let bytes = serde_json::to_vec_pretty(self)?;
        std::fs::write(&staged, bytes)
            .and_then(|()| std::fs::rename(&staged, &path))
            .map_err(|e| {
                let _ = std::fs::remove_file(&staged);
                Error::Invalid(format!("could not write {}: {e}", path.display()))
            })
    }

    /// Every build on record is a development build, and one of them created the home. A
    /// release build that created, adopted or migrated the home ends this.
    fn development_only(&self) -> bool {
        self.created.is_some()
            && [&self.created, &self.adopted, &self.migrated]
                .into_iter()
                .flatten()
                .all(|stamp| stamp.build == DEVELOPMENT)
    }

    /// The binary the home belongs to: the one that last migrated it, else the one that
    /// adopted it, else the one that created it. Adoption only happens while no path is on
    /// record, so this is also the most recent of them.
    fn binary(&self) -> Option<&str> {
        [&self.migrated, &self.adopted, &self.created]
            .into_iter()
            .flatten()
            .find_map(|stamp| stamp.binary.as_deref())
    }
}

/// Why this build will not migrate a home without `--dev-migrate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Blocked {
    /// A development build, and the home is not one that only development builds made.
    DevelopmentBuild,
    /// A release build whose executable is not the one the home belongs to.
    OtherBinary { recorded: String, this: String },
}

impl Blocked {
    /// The one rule the store and `another doctor` both ask. A release build meeting a home
    /// with no binary on record is not blocked: it migrates and becomes the home's binary.
    fn check(origin: &HomeOrigin, build: &Build) -> Option<Blocked> {
        if build.allow_dev_migrate {
            return None;
        }
        if build.development {
            return (!origin.development_only()).then_some(Blocked::DevelopmentBuild);
        }
        let recorded = origin.binary()?;
        let this = build.binary_text();
        (this.as_deref() != Some(recorded)).then(|| Blocked::OtherBinary {
            recorded: recorded.to_string(),
            this: this.unwrap_or_else(|| "unknown path".to_string()),
        })
    }

    fn refusal(&self, home: &Path, current: i64, latest: i64) -> Error {
        let home = home.display();
        Error::Invalid(match self {
            Blocked::DevelopmentBuild => format!(
                "this development build of another will not migrate the database in {home} from schema v{current} to v{latest}: a development build did not create this home, or a release build has used it since; use the installed another for it, or give this build its own home with ANOTHER_HOME=$(mktemp -d); to migrate it anyway, pass --dev-migrate (a backup is taken first)"
            ),
            Blocked::OtherBinary { recorded, this } => format!(
                "this another binary ({this}) will not migrate the database in {home} from schema v{current} to v{latest}: the home belongs to the binary at {recorded}, which created, adopted or last migrated it; upgrade that binary in place, or pass --dev-migrate to migrate with this one (a backup is taken first)"
            ),
        })
    }
}

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

/// What [`Store::inspect`] found.
#[derive(Debug, Clone, Serialize)]
pub struct DbInspection {
    /// The database's schema version (`user_version`).
    pub version: i64,
    /// The schema version this build migrates to.
    pub expected: i64,
    /// `ok`, or SQLite's first `quick_check` complaint.
    pub quick_check: String,
    /// The tables and indexes are the ones this build has for `version`. False for a negative
    /// version and for a schema a different build changed; not judged for a newer version.
    pub known: bool,
    /// When `known` is false: the tables whose columns, indexes or foreign keys differ.
    pub differs: Vec<String>,
    /// Why a normal open by this build would refuse to migrate the database, when a migration
    /// is pending and that is so.
    pub blocked: Option<Blocked>,
    /// Which build created the database and which last migrated it.
    pub origin: HomeOrigin,
    /// The most recent backup in the home's backup directory that this build can open.
    pub latest_backup: Option<PathBuf>,
}

impl Store {
    /// Open (creating if needed) the node database inside `home`.
    pub fn open(home: &Path) -> Result<Store> {
        Self::open_with_clock(home, Clock::System)
    }

    pub fn open_with_clock(home: &Path, clock: Clock) -> Result<Store> {
        Self::open_on_disk(home, clock, Build::current())
    }

    /// [`Store::open`] as `build`, so tests can exercise the upgrade rules of a release build
    /// and of a development build from one binary.
    #[cfg(feature = "test-support")]
    pub fn open_as(home: &Path, build: Build) -> Result<Store> {
        Self::open_on_disk(home, Clock::System, build)
    }

    fn open_on_disk(home: &Path, clock: Clock, build: Build) -> Result<Store> {
        std::fs::create_dir_all(home)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700))?;
        }
        let path = home.join(DB_FILE);
        let conn = Connection::open(&path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            // Switching to WAL rewrites the file's header, so a database this build will
            // refuse is refused first. Every home a build made is already in WAL mode.
            refuse(&conn, Some(home), &HomeOrigin::read(home), &build)?;
            use_wal(&conn)?;
        }
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(conn, clock, Some(path), build)
    }

    /// Look at the database in `home` for `another doctor`: its schema version against this
    /// build's, and SQLite's `quick_check`. The file is opened read-only, so nothing is
    /// created, migrated or written. SQLite may still add the empty `-shm` / `-wal` files any
    /// reader of a WAL database makes. `NotFound` when there is no database file.
    pub fn inspect(home: &Path, busy: std::time::Duration) -> Result<DbInspection> {
        Self::inspect_for(home, busy, &Build::current())
    }

    /// [`Store::inspect`] as `build`.
    #[cfg(feature = "test-support")]
    pub fn inspect_as(
        home: &Path,
        busy: std::time::Duration,
        build: &Build,
    ) -> Result<DbInspection> {
        Self::inspect_for(home, busy, build)
    }

    fn inspect_for(home: &Path, busy: std::time::Duration, build: &Build) -> Result<DbInspection> {
        let path = home.join(DB_FILE);
        if !path.is_file() {
            return Err(Error::NotFound(format!(
                "no database at {}",
                path.display()
            )));
        }
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(busy)?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let quick_check: String = conn.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
        let expected = MIGRATIONS.len() as i64;
        // Doctor is the thorough one: it reads the catalog itself, whatever the record says.
        let known = version > expected || known_schema(&conn, version, None)?;
        let differs = if known {
            Vec::new()
        } else {
            differing_tables(&conn, version)
        };
        let origin = HomeOrigin::read(home);
        // The same question, in the same order, as a normal open asks before migrating.
        let blocked = if known && version > 0 && version < expected {
            Blocked::check(&origin, build)
        } else {
            None
        };
        Ok(DbInspection {
            version,
            expected,
            quick_check,
            known,
            differs,
            blocked,
            origin,
            latest_backup: latest_backup(home, expected),
        })
    }

    /// Open an existing database for a hook that runs on every tool call: nothing is created,
    /// nothing is migrated, opening writes nothing, and `busy` bounds every wait for a lock
    /// so a busy database fails fast instead of stalling the agent. Fails when the database
    /// is missing or its schema is not current; a normal [`Store::open`] fixes both.
    pub fn open_quick(home: &Path, busy: std::time::Duration) -> Result<Store> {
        Self::open_existing(home, busy, false)
    }

    /// [`Store::open_quick`] on a read-only connection, for `another doctor`: every read
    /// works, any write fails, and closing it cannot checkpoint the WAL into the main
    /// file (a read-write connection that closes may do that, which changes the file's
    /// bytes though not its contents).
    pub fn open_read_only(home: &Path, busy: std::time::Duration) -> Result<Store> {
        Self::open_existing(home, busy, true)
    }

    fn open_existing(home: &Path, busy: std::time::Duration, read_only: bool) -> Result<Store> {
        let path = home.join(DB_FILE);
        let mode = if read_only {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        };
        let conn = Connection::open_with_flags(
            &path,
            mode | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(busy)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        if !read_only {
            conn.pragma_update(None, "synchronous", "NORMAL")?;
        }
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > MIGRATIONS.len() as i64 {
            // A normal open refuses this too; only a newer build can use the database.
            return Err(newer_schema(version));
        }
        if version != MIGRATIONS.len() as i64 {
            return Err(Error::Invalid(format!(
                "database schema is v{version}, this build expects v{}; a normal open migrates it",
                MIGRATIONS.len()
            )));
        }
        // The table check a normal open makes. The record of the last check lets a hook skip
        // the catalog queries; without it, or after a schema change, they run here.
        if !known_schema(&conn, version, Some(&HomeOrigin::read(home)))? {
            return Err(unknown_schema(&conn, version));
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
        Self::init(Connection::open_in_memory()?, clock, None, Build::current())
    }

    fn init(
        mut conn: Connection,
        clock: Clock,
        path: Option<PathBuf>,
        build: Build,
    ) -> Result<Store> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&mut conn, path.as_deref(), &build, clock.now())?;
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
    // Durable, node-local subscriptions created by blocked file edits.
    r#"
CREATE TABLE hold_waits (
    agent_id       TEXT NOT NULL REFERENCES agents(agent_id),
    project_id     TEXT NOT NULL REFERENCES projects(project_id),
    path           TEXT NOT NULL,
    blockers       TEXT NOT NULL,
    holder_id      TEXT NOT NULL REFERENCES agents(agent_id),
    retry_at       INTEGER NOT NULL,
    backoff_seconds INTEGER NOT NULL,
    PRIMARY KEY (agent_id, project_id, path)
);
"#,
    r#"
CREATE TABLE request_holds (
    request_id TEXT PRIMARY KEY REFERENCES requests(request_id),
    hold_id    TEXT NOT NULL REFERENCES holds(hold_id)
);
CREATE INDEX request_holds_by_hold ON request_holds(hold_id);
ALTER TABLE announcements ADD COLUMN fingerprint TEXT;
ALTER TABLE announcements ADD COLUMN duplicate_of TEXT REFERENCES messages(message_id);
CREATE INDEX announcements_by_fingerprint ON announcements(agent_id, fingerprint, announced_at);
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

/// A database a newer build has migrated: this build must not touch it. The normal and the
/// quick open say the same thing, so the CLI and the hook log give the same fix.
fn newer_schema(version: i64) -> Error {
    Error::Invalid(format!(
        "database schema v{version} is newer than this another build supports (v{}); upgrade another, or stop every another process and restore the backup taken before the migration (docs/DOCTOR.md)",
        MIGRATIONS.len()
    ))
}

/// Bring the database to this build's schema, or refuse and write nothing. All pending steps
/// run in one write transaction, so the backup taken at its start is exactly the database the
/// migration replaced, and a failure leaves the old schema whole.
fn migrate(conn: &mut Connection, db: Option<&Path>, build: &Build, now: i64) -> Result<()> {
    let latest = MIGRATIONS.len() as i64;
    // An in-memory database has no home: nothing to protect and nowhere to record it.
    let home = db.and_then(Path::parent);
    // Read the version inside the write transaction: another process may have migrated
    // while this one waited for the lock (several sessions opening a new home at once).
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut origin = home.map(HomeOrigin::read).unwrap_or_default();
    let current = refuse(&tx, home, &origin, build)?;
    if current == latest {
        drop(tx);
        if let Some(home) = home {
            remember(conn, home, origin, build, now);
        }
        return Ok(());
    }
    let existing = db.zip(home).filter(|_| current > 0);
    if let Some((db, home)) = existing {
        backup(db, home, current, now).map_err(|e| {
            Error::Invalid(format!(
                "the backup to {} failed, so the database was not migrated: {e}",
                home.join(BACKUP_DIR).display()
            ))
        })?;
    }
    for sql in &MIGRATIONS[current as usize..] {
        tx.execute_batch(sql)?;
    }
    tx.pragma_update(None, "user_version", latest)?;
    if let Some(home) = home {
        let stamp = Some(build.stamp(latest, now));
        if current == 0 {
            origin = HomeOrigin {
                created: stamp,
                ..HomeOrigin::default()
            };
        } else {
            origin.migrated = stamp;
        }
        origin.verified = verified(&tx, latest)?;
        origin.write(home)?;
    }
    tx.commit()?;
    // Only now that the migration is committed: a failed one deletes nothing.
    if let Some((_, home)) = existing {
        prune_backups(home);
    }
    Ok(())
}

/// Every reason this build will not use the database, checked without writing: a newer or
/// invalid schema version, tables a different build changed, and a pending migration this
/// build may not make. Returns the schema version.
fn refuse(
    conn: &Connection,
    home: Option<&Path>,
    origin: &HomeOrigin,
    build: &Build,
) -> Result<i64> {
    let latest = MIGRATIONS.len() as i64;
    let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if current > latest {
        return Err(newer_schema(current));
    }
    if current < 0 {
        return Err(Error::Invalid(format!(
            "database schema version {current} is not valid: this is not an another database, or it was edited by hand; check that --home or ANOTHER_HOME points at an another home"
        )));
    }
    if !known_schema(conn, current, Some(origin))? {
        return Err(unknown_schema(conn, current));
    }
    // A new database (v0) is created, not migrated: there is nothing in it to protect.
    if let (Some(home), true) = (home, current > 0 && current < latest) {
        if let Some(blocked) = Blocked::check(origin, build) {
            return Err(blocked.refusal(home, current, latest));
        }
    }
    Ok(current)
}

/// After a normal open of a current database: note that its tables were checked, so later
/// opens can skip the catalog queries, and let a release build take on a home that has no
/// binary on record. Best effort: the open has already succeeded, and a record that cannot
/// be written only means the next open tries again.
fn remember(conn: &Connection, home: &Path, mut origin: HomeOrigin, build: &Build, now: i64) {
    let latest = MIGRATIONS.len() as i64;
    let before = origin.clone();
    if !build.development && build.binary.is_some() && origin.binary().is_none() {
        origin.adopted = Some(build.stamp(latest, now));
    }
    if let Ok(Some(mark)) = verified(conn, latest) {
        origin.verified = Some(mark);
    }
    if origin != before {
        let _ = origin.write(home);
    }
}

/// The mark that says the tables of schema `version` were checked at the current cookie.
fn verified(conn: &Connection, version: i64) -> Result<Option<Verified>> {
    let Some(digest) = frozen_digest(version) else {
        return Ok(None);
    };
    Ok(Some(Verified {
        schema: version,
        cookie: conn.pragma_query_value(None, "schema_version", |r| r.get(0))?,
        digest: digest.to_string(),
    }))
}

/// Copy the database, as last committed, to a new file under the home's backup directory and
/// check the copy. The caller holds the write lock, so no writer can slip in between the
/// copy and the migration; the copy is made through a second, read-only connection.
fn backup(db: &Path, home: &Path, version: i64, now: i64) -> Result<PathBuf> {
    let dir = home.join(BACKUP_DIR);
    // A link is neither followed nor chmodded: the backup belongs in the home.
    if dir
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        return Err(Error::Invalid(format!(
            "{} is a symbolic link; replace it with a directory",
            dir.display()
        )));
    }
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let taken = chrono::DateTime::from_timestamp_millis(now).map_or_else(
        || now.to_string(),
        |t| t.format("%Y%m%dT%H%M%SZ").to_string(),
    );
    let mut path = dir.join(format!("another-v{version}-{taken}.db"));
    let mut n = 1;
    while path.exists() {
        n += 1;
        path = dir.join(format!("another-v{version}-{taken}-{n}.db"));
    }
    let target = path
        .to_str()
        .ok_or_else(|| Error::Invalid(format!("{} is not valid UTF-8", path.display())))?;
    let read_only = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    Connection::open_with_flags(db, read_only)?.execute("VACUUM INTO ?1", [target])?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    let copy = Connection::open_with_flags(&path, read_only)?;
    let check: String = copy.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
    let saved: i64 = copy.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if check != "ok" || saved != version {
        return Err(Error::Invalid(format!(
            "{} did not verify (quick_check {check}, schema v{saved})",
            path.display()
        )));
    }
    Ok(path)
}

/// The backups in the home's backup directory, oldest first: regular files named as
/// [`backup`] names them (`another-v<schema>-<time>.db`), each with its schema version.
/// Anything else kept there is not a backup and is never offered or deleted.
fn backups(home: &Path) -> Vec<(std::time::SystemTime, PathBuf, i64)> {
    let Ok(entries) = std::fs::read_dir(home.join(BACKUP_DIR)) else {
        return Vec::new();
    };
    let mut found: Vec<_> = entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let name = path.file_name()?.to_str()?;
            let (schema, taken) = name
                .strip_prefix("another-v")?
                .strip_suffix(".db")?
                .split_once('-')?;
            let schema: i64 = schema.parse().ok()?;
            let meta = path.symlink_metadata().ok()?;
            (meta.is_file() && !taken.is_empty()).then_some((meta.modified().ok()?, path, schema))
        })
        .collect();
    found.sort();
    found
}

/// The newest backup whose schema a build at `schema` can open.
fn latest_backup(home: &Path, schema: i64) -> Option<PathBuf> {
    backups(home)
        .into_iter()
        .rev()
        .find(|(_, _, saved)| *saved <= schema)
        .map(|(_, path, _)| path)
}

/// Keep the newest [`BACKUPS_KEPT`] backups and delete the rest. A file that cannot be
/// deleted is left for the next migration.
fn prune_backups(home: &Path) {
    let found = backups(home);
    let excess = found.len().saturating_sub(BACKUPS_KEPT);
    for (_, path, _) in &found[..excess] {
        let _ = std::fs::remove_file(path);
    }
}

/// Apply the first `version` migrations to an empty database and set its schema version.
/// Tests use it to build a home as an earlier build left it.
pub fn apply_schema(conn: &Connection, version: i64) -> Result<()> {
    let steps = usize::try_from(version)
        .ok()
        .and_then(|n| MIGRATIONS.get(..n))
        .ok_or_else(|| Error::Invalid(format!("this build has no schema v{version}")))?;
    for sql in steps {
        conn.execute_batch(sql)?;
    }
    Ok(conn.pragma_update(None, "user_version", version)?)
}

/// SHA-256 of the [`catalog`] at each schema version, from v0 (an empty database) to the
/// latest. Frozen: a released schema never changes shape, and a new migration appends its
/// line (the test `every_schema_version_has_its_frozen_catalog` prints the value).
const CATALOGS: &[&str] = &[
    "6a3cf5192354f71615ac51034b3e97c20eda99643fcaf5bbe6d41ad59bd12167",
    "ee4747a82f67486ffea3cf9f95ddbd95978894050fa2173b5ea2f558fb2bf578",
    "9c82ddd60688bde41664cf1a41191f25ecf82b2c4deb90e7f6ca83aac00389a7",
    "973ab85a0a7a52baf3f116374f6753c72d64ac98f7ee7888ea635eb78c335a8e",
    "86cbd3f6824e4617935e0697036d4432d7db79c3e09d22d313981d0e9a0e0e27",
    "476f0ae405cbe84094cb81bafc4302d3cbfcec10945d010851489255dc9895c5",
    "58c89ec89e9df0a9b008eb299fc65b191cc2974b675f3e9acb1eb96f4ff82e08",
    "7ab9be753f330b0a7042318ef9897363c049edc6a6644dd22d1183977b57265e",
    "f85d51188f29f23863c5e90224d308abd1150bb9ed019245681e7d6900614a73",
    "3263f7df9c4fd6613f7c466cbb01a84962fd4bd1635b3f87d96f2751b2d6f372",
    "5b76a246c4ba93c3edd4c1a7d03fe0f19f43aadceed5901dfa0ba6b439be9c54",
];

/// The digest this build holds for schema `version`.
fn frozen_digest(version: i64) -> Option<&'static str> {
    let version = usize::try_from(version).ok()?;
    CATALOGS.get(version).copied()
}

/// Whether the database's tables and indexes are the ones this build has for `version`.
/// With the home's record, a check already made for this version, this digest and the
/// database's current schema cookie is taken as made; `None` always reads the catalog.
fn known_schema(conn: &Connection, version: i64, origin: Option<&HomeOrigin>) -> Result<bool> {
    let Some(digest) = frozen_digest(version) else {
        return Ok(false);
    };
    if let Some(mark) = origin.and_then(|origin| origin.verified.as_ref()) {
        if mark.schema == version && mark.digest == digest {
            let cookie: i64 = conn.pragma_query_value(None, "schema_version", |r| r.get(0))?;
            if mark.cookie == cookie {
                return Ok(true);
            }
        }
    }
    Ok(digest == catalog_digest(conn)?)
}

fn unknown_schema(conn: &Connection, version: i64) -> Error {
    let tables = differing_tables(conn, version);
    let which = if tables.is_empty() {
        String::new()
    } else {
        format!(" (tables that differ: {})", tables.join(", "))
    };
    Error::Invalid(format!(
        "database schema v{version} does not match what this another build has for v{version}: a different build changed it{which}, and this build will not use it; run another doctor, then use the build that wrote it or stop every another process and restore a backup (docs/DOCTOR.md)"
    ))
}

/// The tables whose columns, indexes or foreign keys are not what this build's migrations
/// give schema `version`, by name. Only used once a mismatch is known, so the cost of
/// building that schema in memory is paid on the refusal path alone. Empty when the
/// comparison cannot be made.
fn differing_tables(conn: &Connection, version: i64) -> Vec<String> {
    let compare = || -> Result<Vec<String>> {
        let ours = Connection::open_in_memory()?;
        apply_schema(&ours, version)?;
        let (theirs, ours) = (shape(conn)?, shape(&ours)?);
        let mut tables: Vec<String> = theirs
            .iter()
            .filter(|row| !ours.contains(row))
            .chain(ours.iter().filter(|row| !theirs.contains(row)))
            .map(|(_, table, _)| table.clone())
            .collect();
        tables.sort();
        tables.dedup();
        Ok(tables)
    };
    compare().unwrap_or_default()
}

/// The shape of the schema, one query each: every table's columns, its indexes (those of
/// `UNIQUE` and primary keys included) with their columns, and its foreign keys. These read
/// the pragmas, not the stored SQL text, which differs between SQLite versions for the same
/// schema. Triggers and views are not part of it: no migration creates one.
const SHAPE: &[&str] = &[
    r#"SELECT m.name, c.cid, c.name, c.type, c."notnull", c.dflt_value, c.pk
       FROM sqlite_master m JOIN pragma_table_info(m.name) c
       WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%' ORDER BY m.name, c.cid"#,
    r#"SELECT m.name, l.name, l."unique", l.origin, l.partial, i.seqno, i.name
       FROM sqlite_master m JOIN pragma_index_list(m.name) l JOIN pragma_index_info(l.name) i
       WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%' ORDER BY m.name, l.name, i.seqno"#,
    r#"SELECT m.name, f.id, f.seq, f."table", f."from", f."to", f.on_update, f.on_delete
       FROM sqlite_master m JOIN pragma_foreign_key_list(m.name) f
       WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%' ORDER BY m.name, f.id, f.seq"#,
];

/// SHA-256 over the rows of [`SHAPE`], so two databases built by different paths compare
/// equal exactly when their schemas have the same shape.
fn catalog_digest(conn: &Connection) -> Result<String> {
    let mut text = String::new();
    let mut query = 0;
    for (index, _, row) in shape(conn)? {
        // A blank line closes each query's rows, also when a query has none.
        while query < index {
            text.push('\n');
            query += 1;
        }
        text.push_str(&row);
        text.push('\n');
    }
    for _ in query..SHAPE.len() {
        text.push('\n');
    }
    Ok(crate::crypto::sha256_hex(text.as_bytes()))
}

/// The rows of [`SHAPE`] in order: which query, the table the row describes, and the row as
/// text.
fn shape(conn: &Connection) -> Result<Vec<(usize, String, String)>> {
    use rusqlite::types::ValueRef;
    let mut out = Vec::new();
    for (index, sql) in SHAPE.iter().enumerate() {
        let mut stmt = conn.prepare(sql)?;
        let columns = stmt.column_count();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let mut text = String::new();
            for i in 0..columns {
                match row.get_ref(i)? {
                    ValueRef::Null => text.push('-'),
                    ValueRef::Integer(n) => text.push_str(&n.to_string()),
                    ValueRef::Real(n) => text.push_str(&n.to_string()),
                    ValueRef::Text(t) | ValueRef::Blob(t) => {
                        text.push_str(&format!("{:?}", String::from_utf8_lossy(t)))
                    }
                }
                text.push(',');
            }
            out.push((index, row.get::<_, String>(0)?, text));
        }
    }
    Ok(out)
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

#[cfg(test)]
mod upgrade_tests {
    use super::*;
    use crate::{AcquireHolds, NewRequest, Program, RegisterAgent, SendMessage};

    /// Every table and index with its columns, types, constraints and foreign keys, in a
    /// stable order, so two databases built by different paths can be compared.
    fn catalog(conn: &Connection) -> Vec<String> {
        let mut out = Vec::new();
        let objects: Vec<(String, String, String)> = conn
            .prepare(
                "SELECT type, name, tbl_name FROM sqlite_master
                 WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        for (kind, name, table) in objects {
            let (columns, keys) = match kind.as_str() {
                "table" => (
                    format!("PRAGMA table_info(\"{name}\")"),
                    Some(format!("PRAGMA foreign_key_list(\"{name}\")")),
                ),
                _ => (format!("PRAGMA index_info(\"{name}\")"), None),
            };
            let mut stmt = conn.prepare(&columns).unwrap();
            let n = stmt.column_count();
            let rows: Vec<String> = stmt
                .query_map([], |r| {
                    (0..n)
                        .map(|i| {
                            r.get::<_, rusqlite::types::Value>(i)
                                .map(|v| format!("{v:?}"))
                        })
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .map(|v| v.join(","))
                })
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            out.push(format!("{kind} {name} on {table}: {}", rows.join(" | ")));
            if let Some(keys) = keys {
                let mut stmt = conn.prepare(&keys).unwrap();
                let fks: Vec<String> = stmt
                    .query_map([], |r| {
                        Ok(format!(
                            "{}->{}.{}",
                            r.get::<_, String>(3)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, Option<String>>(4)?.unwrap_or_default()
                        ))
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap();
                out.push(format!("fk {name}: {}", fks.join(" ")));
            }
        }
        out
    }

    fn version(conn: &Connection) -> i64 {
        conn.pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    }

    /// The catalog of a database built by applying the first `n` migrations directly.
    fn catalog_at(n: usize) -> Vec<String> {
        let conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn, n as i64).unwrap();
        catalog(&conn)
    }

    /// The frozen schema catalog: one digest per schema version. A change to the shape of a
    /// released schema fails here, as does a migration added without its line.
    #[test]
    fn every_schema_version_has_its_frozen_catalog() {
        let now: Vec<String> = (0..=MIGRATIONS.len())
            .map(|n| {
                let conn = Connection::open_in_memory().unwrap();
                apply_schema(&conn, n as i64).unwrap();
                catalog_digest(&conn).unwrap()
            })
            .collect();
        assert_eq!(
            CATALOGS, now,
            "CATALOGS must list these digests, v0 first; only ever append to it"
        );
    }

    #[test]
    fn schema_versions_after_v8_are_hold_waits_then_request_holds() {
        // v1-v8 are main's released schema. v9 and v10 belong to the hold fixes (D62, D63);
        // a later migration must append after them, never reuse either number.
        assert_eq!(MIGRATIONS.len(), 10);
        let v8 = catalog_at(8);
        let v9 = catalog_at(9);
        let v10 = catalog_at(10);
        let added = |from: &[String], to: &[String]| -> Vec<String> {
            to.iter().filter(|o| !from.contains(o)).cloned().collect()
        };
        let names = |objects: Vec<String>| -> Vec<String> {
            objects
                .into_iter()
                .map(|o| o.split(':').next().unwrap().to_string())
                .collect()
        };
        assert_eq!(
            names(added(&v8, &v9)),
            ["table hold_waits on hold_waits", "fk hold_waits"]
        );
        assert_eq!(
            names(added(&v9, &v10)),
            [
                "index announcements_by_fingerprint on announcements",
                "index request_holds_by_hold on request_holds",
                "table announcements on announcements",
                "fk announcements",
                "table request_holds on request_holds",
                "fk request_holds",
            ]
        );
        let announcements = v10
            .iter()
            .find(|o| o.starts_with("table announcements "))
            .unwrap();
        assert!(
            announcements.contains("Text(\"fingerprint\")"),
            "{announcements}"
        );
        assert!(
            announcements.contains("Text(\"duplicate_of\")"),
            "{announcements}"
        );
    }

    #[test]
    fn a_v8_home_upgrades_to_v10_and_keeps_its_data() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let root = dir.path().join("proj");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn a() {}\n").unwrap();

        // Populate a home through the API, using only what v8 already had.
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
        let project = store
            .ensure_project("test", root.to_str().unwrap())
            .unwrap()
            .abs_path;
        let hold = store
            .acquire_holds(AcquireHolds {
                agent: "alice".into(),
                project: project.clone(),
                paths: vec!["src/a.rs".into()],
                ttl_seconds: Some(600),
                exclusive: true,
                reason: Some("work".into()),
                repo_wide: false,
            })
            .unwrap()
            .granted
            .remove(0);
        let legacy = store
            .send_message(SendMessage {
                from: "bob".into(),
                to: vec!["alice".into()],
                subject: "please release src/a.rs".into(),
                body: "a request written before linked release requests existed".into(),
                request: Some(NewRequest::default()),
                ..Default::default()
            })
            .unwrap();
        let legacy_request = legacy.request_id.clone().unwrap();
        assert_eq!(
            store
                .claim_announcement(
                    "alice",
                    std::slice::from_ref(&legacy.message_id),
                    "post_tool",
                    None
                )
                .unwrap()
                .len(),
            1
        );
        drop(store);

        // Take the home back to exactly main's v8 schema, keeping every v8 row.
        let db = Connection::open(home.join(DB_FILE)).unwrap();
        db.execute_batch(
            "DROP TABLE hold_waits;
             DROP TABLE request_holds;
             ALTER TABLE announcements RENAME TO announcements_v10;",
        )
        .unwrap();
        db.execute_batch(MIGRATIONS[7]).unwrap();
        db.execute_batch(
            "INSERT INTO announcements (agent_id, message_id, channel, announced_at)
               SELECT agent_id, message_id, channel, announced_at FROM announcements_v10;
             DROP TABLE announcements_v10;",
        )
        .unwrap();
        db.pragma_update(None, "user_version", 8).unwrap();
        assert_eq!(catalog(&db), catalog_at(8), "the fixture is a true v8 home");
        drop(db);

        // A normal open upgrades it, and the result matches a fresh v10 home.
        let store = Store::open(&home).unwrap();
        let db = Connection::open(home.join(DB_FILE)).unwrap();
        assert_eq!(version(&db), MIGRATIONS.len() as i64);
        assert_eq!(catalog(&db), catalog_at(MIGRATIONS.len()));

        // The backup taken before the upgrade is the v8 home, hold and all.
        let saved = latest_backup(&home, 8).expect("a backup before the migration");
        let saved = Connection::open(saved).unwrap();
        assert_eq!(version(&saved), 8);
        assert_eq!(catalog(&saved), catalog_at(8));
        let held: i64 = saved
            .query_row(
                "SELECT count(*) FROM holds WHERE hold_id = ?1",
                [&hold.hold_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            held, 1,
            "the backup holds the data the migration started from"
        );
        drop(saved);

        let (fingerprint, duplicate): (Option<String>, Option<String>) = db
            .query_row(
                "SELECT fingerprint, duplicate_of FROM announcements WHERE message_id = ?1",
                [&legacy.message_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (fingerprint, duplicate),
            (None, None),
            "old claims gain empty columns"
        );

        // The hold survived, and the v9/v10 behaviour works on the upgraded home.
        let blocked = store
            .guard_edit("bob", &project, &["src/a.rs".into()], "editing (auto)")
            .unwrap();
        assert_eq!(blocked.conflicts.len(), 1);
        assert_eq!(blocked.conflicts[0].held.hold_id, hold.hold_id);
        let waits: i64 = db
            .query_row("SELECT count(*) FROM hold_waits", [], |r| r.get(0))
            .unwrap();
        assert_eq!(waits, 1, "the blocked edit records a durable wait");
        let ask = store.request_hold_release("bob", &hold.hold_id).unwrap();
        let linked = ask.request_id.unwrap();
        let link: String = db
            .query_row(
                "SELECT hold_id FROM request_holds WHERE request_id = ?1",
                [&linked],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(link, hold.hold_id);
        let unlinked: i64 = db
            .query_row(
                "SELECT count(*) FROM request_holds WHERE request_id = ?1",
                [&legacy_request],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            unlinked, 0,
            "a request from before the upgrade is never linked"
        );

        store
            .release_holds("alice", std::slice::from_ref(&hold.hold_id))
            .unwrap();
        let status = |id: &str| -> String {
            db.query_row(
                "SELECT status FROM requests WHERE request_id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(status(&linked), "done");
        assert_eq!(
            status(&legacy_request),
            "open",
            "prose is never inferred as a link"
        );
        let waits: i64 = db
            .query_row("SELECT count(*) FROM hold_waits", [], |r| r.get(0))
            .unwrap();
        assert_eq!(waits, 0, "the release pings and clears the wait");
    }
}
