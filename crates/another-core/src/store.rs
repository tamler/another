//! SQLite store: connection handling, schema migrations, local node identity and the
//! append-only event log that every mutation writes to first.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
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
    busy_timeout: std::time::Duration,
    /// Deadline of the single bounded operation this store was opened for, if any.
    deadline: Option<std::time::Instant>,
    /// SQLite VM steps the bounded operation has used so far.
    steps: Arc<std::sync::atomic::AtomicUsize>,
    identity_uncertain: AtomicBool,
    #[cfg(test)]
    identity_commit_fault: std::sync::atomic::AtomicU8,
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
        let path = home.join(DB_FILE);
        let conn = Connection::open(&path)?;
        require_current_or_empty(&conn)?;
        if conn.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))? != 0 {
            verify_v8_schema(&conn, None)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700))?;
        }
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
        Self::upgrade_offline_with_parent_sync(home, backup_dir, writer_fence_confirmed, |parent| {
            std::fs::File::open(parent)?.sync_all()
        })
    }

    #[cfg(unix)]
    fn upgrade_offline_with_parent_sync(
        home: &Path,
        backup_dir: &Path,
        writer_fence_confirmed: bool,
        sync_parent: impl FnOnce(&Path) -> std::io::Result<()>,
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
        sync_parent(&backup_parent).map_err(|e| {
            Error::Invalid(format!(
                "backup parent directory sync failed ({e}); source was not migrated; preserve the backup and writer fence"
            ))
        })?;
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
        Self::open_existing_inner(home, busy, None).map_err(identity_open_error)
    }

    /// Open an existing database for one bounded operation (a hook run or `session exec`).
    /// Like [`Store::open_quick`] it creates and migrates nothing. In addition every SQLite
    /// statement on the returned store, from schema validation at open onward, stops at
    /// `deadline` or after [`OPERATION_STEP_LIMIT`] VM steps, and lock waits never extend
    /// past `deadline`.
    pub fn open_existing(
        home: &Path,
        busy: std::time::Duration,
        deadline: std::time::Instant,
    ) -> Result<Store> {
        match Self::open_existing_inner(home, busy, Some(deadline)) {
            Err(_) if std::time::Instant::now() >= deadline => {
                Err(Error::IdentityScanBudgetExceeded("deadline".into()))
            }
            other => other.map_err(identity_open_error),
        }
    }

    fn open_existing_inner(
        home: &Path,
        busy: std::time::Duration,
        deadline: Option<std::time::Instant>,
    ) -> Result<Store> {
        use rusqlite::OpenFlags;
        let path = home.join(DB_FILE);
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let steps = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let busy = match deadline {
            Some(deadline) => {
                install_operation_budget(&conn, deadline, steps.clone())?;
                busy.min(deadline.saturating_duration_since(std::time::Instant::now()))
            }
            None => busy,
        };
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
        verify_v8_schema(&conn, deadline)?;
        let (node_id, key) = load_existing_local_node(&conn)?;
        Ok(Store {
            inner: Arc::new(Inner {
                conn: Mutex::new(conn),
                node_id,
                key,
                clock: Clock::System,
                path: Some(path),
                busy_timeout: busy,
                deadline,
                steps,
                identity_uncertain: AtomicBool::new(false),
                #[cfg(test)]
                identity_commit_fault: std::sync::atomic::AtomicU8::new(0),
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
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        migrate(&mut conn)?;
        let (node_id, key) = if version == 0 {
            ensure_local_node(&mut conn, clock.now())?
        } else {
            load_existing_local_node(&conn)?
        };
        Ok(Store {
            inner: Arc::new(Inner {
                conn: Mutex::new(conn),
                node_id,
                key,
                clock,
                path,
                busy_timeout: std::time::Duration::from_secs(5),
                deadline: None,
                steps: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                identity_uncertain: AtomicBool::new(false),
                #[cfg(test)]
                identity_commit_fault: std::sync::atomic::AtomicU8::new(0),
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
        if self.inner.identity_uncertain.load(Ordering::Acquire) {
            return Err(Error::Invalid(
                "binding commit outcome uncertain; reopen and reconcile identity".into(),
            ));
        }
        f(&conn)
    }

    /// Run `f` inside an immediate write transaction; commits on `Ok`.
    pub(crate) fn write<T>(&self, f: impl FnOnce(&Tx<'_>) -> Result<T>) -> Result<T> {
        let mut conn = self.lock();
        if self.inner.identity_uncertain.load(Ordering::Acquire) {
            return Err(Error::Invalid(
                "binding commit outcome uncertain; reopen and reconcile identity".into(),
            ));
        }
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Every writer honors the durable identity fence: an identity commit whose outcome
        // was not confirmed is settled here, under the write lock, before anything else.
        reconcile_fences(&tx, self.inner.path.as_deref())?;
        let ctx = Tx {
            tx,
            node_id: &self.inner.node_id,
            now: self.inner.clock.now(),
            key: &self.inner.key,
            identity_op: std::cell::RefCell::new(None),
        };
        let out = f(&ctx)?;
        ctx.tx.commit()?;
        Ok(out)
    }

    /// Settle identity operations whose commit outcome was never confirmed (a failed or
    /// interrupted commit, or a crash before the fence was cleared). Runs under the write
    /// lock, so each attempted transaction has ended and is either fully present or fully
    /// rolled back. Every write does this implicitly; this entry point reports the outcomes.
    pub fn reconcile_identity_operations(&self) -> Result<Vec<ReconciledOperation>> {
        let mut conn = self.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let settled = reconcile_fences(&tx, self.inner.path.as_deref())?;
        tx.commit()?;
        Ok(settled)
    }

    /// Run the bounded binding scan and uniqueness decision in one immediate transaction.
    pub(crate) fn write_identity<T>(
        &self,
        f: impl FnOnce(&Tx<'_>, std::time::Instant) -> Result<T>,
    ) -> Result<T> {
        self.write_identity_with_step_limit(200_000, f)
    }

    fn write_identity_with_step_limit<T>(
        &self,
        step_limit: usize,
        f: impl FnOnce(&Tx<'_>, std::time::Instant) -> Result<T>,
    ) -> Result<T> {
        use rusqlite::limits::Limit;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::{Duration, Instant};

        // One identity phase gets two seconds, never more than its operation has left.
        let phase = Instant::now() + Duration::from_secs(2);
        let deadline = self.inner.deadline.map_or(phase, |end| end.min(phase));
        let mut conn = self.lock();
        if self.inner.identity_uncertain.load(Ordering::Acquire) {
            let path = self.inner.path.as_ref().ok_or_else(|| {
                Error::IdentityScanBudgetExceeded("cannot reopen uncertain in-memory store".into())
            })?;
            let replacement = Connection::open_with_flags(
                path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                    | rusqlite::OpenFlags::SQLITE_OPEN_URI
                    | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|_| {
                Error::IdentityScanBudgetExceeded("cannot reopen uncertain store".into())
            })?;
            replacement
                .busy_timeout(self.inner.busy_timeout)
                .map_err(|_| {
                    Error::IdentityScanBudgetExceeded("cannot configure reopened store".into())
                })?;
            replacement
                .pragma_update(None, "foreign_keys", "ON")
                .map_err(|_| {
                    Error::IdentityScanBudgetExceeded("cannot configure reopened store".into())
                })?;
            replacement
                .pragma_update(None, "synchronous", "NORMAL")
                .map_err(|_| {
                    Error::IdentityScanBudgetExceeded("cannot configure reopened store".into())
                })?;
            let (node, key) = load_existing_local_node(&replacement).map_err(|_| {
                Error::IdentityScanBudgetExceeded("cannot verify reopened store".into())
            })?;
            if node != self.inner.node_id
                || crate::crypto::public_of(&key) != crate::crypto::public_of(&self.inner.key)
            {
                return Err(Error::Invalid(
                    "node changed while reconciling identity".into(),
                ));
            }
            *conn = replacement;
            self.inner
                .identity_uncertain
                .store(false, Ordering::Release);
        }
        if Instant::now() >= deadline {
            return Err(Error::IdentityScanBudgetExceeded("deadline".into()));
        }
        conn.busy_timeout(
            self.inner
                .busy_timeout
                .min(deadline.saturating_duration_since(Instant::now())),
        )?;
        let previous_limit = conn.set_limit(Limit::SQLITE_LIMIT_LENGTH, 8192)?;
        if conn.limit(Limit::SQLITE_LIMIT_LENGTH)? != 8192 {
            conn.set_limit(Limit::SQLITE_LIMIT_LENGTH, previous_limit)?;
            conn.busy_timeout(self.inner.busy_timeout)?;
            return Err(Error::IdentityScanBudgetExceeded(
                "SQLite length limit unavailable".into(),
            ));
        }
        let steps = Arc::new(AtomicUsize::new(0));
        let progress_steps = steps.clone();
        conn.progress_handler(
            1,
            Some(move || {
                progress_steps.fetch_add(1, Ordering::Relaxed) + 1 >= step_limit
                    || Instant::now() >= deadline
            }),
        )?;
        // Identity commits are synced before they are reported, so a confirmed binding
        // survives power loss.
        if conn.pragma_update(None, "synchronous", "FULL").is_err() {
            // Force a fresh connection before this one is used again.
            self.inner.identity_uncertain.store(true, Ordering::Release);
            return Err(Error::IdentityScanBudgetExceeded(
                "identity connection state uncertain".into(),
            ));
        }
        let result = (|| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            verify_v8_schema(&tx, Some(deadline))?;
            if Instant::now() >= deadline {
                return Err(Error::IdentityScanBudgetExceeded("deadline".into()));
            }
            // Settle any earlier identity operation before deciding anything new.
            reconcile_fences(&tx, self.inner.path.as_deref())?;
            let ctx = Tx {
                tx,
                node_id: &self.inner.node_id,
                now: self.inner.clock.now(),
                key: &self.inner.key,
                identity_op: std::cell::RefCell::new(None),
            };
            let out = f(&ctx, deadline)?;
            if Instant::now() >= deadline {
                return Err(Error::IdentityScanBudgetExceeded("deadline".into()));
            }
            // A new binding is fenced on disk before its commit is attempted. If publishing
            // fails nothing was committed: the transaction rolls back on drop.
            let fence = match (ctx.identity_op.borrow_mut().take(), &self.inner.path) {
                (Some(operation), Some(path)) => Some(publish_fence(path, &operation)?),
                _ => None,
            };
            #[cfg(test)]
            let fault = self.inner.identity_commit_fault.swap(0, Ordering::AcqRel);
            #[cfg(not(test))]
            let fault = 0;
            let commit_result = if fault == 2 {
                drop(ctx);
                Err(())
            } else {
                ctx.tx.commit().map_err(|_| ())
            };
            if commit_result.is_err() || fault == 1 {
                // The fence stays on disk: every later writer, in this or any process,
                // settles this exact operation before it may write.
                self.inner.identity_uncertain.store(true, Ordering::Release);
                return Err(Error::IdentityScanBudgetExceeded(
                    "binding commit outcome uncertain".into(),
                ));
            }
            if let Some(fence) = &fence {
                clear_fence(fence);
            }
            if Instant::now() >= deadline {
                return Err(Error::IdentityScanBudgetExceeded(
                    "deadline passed while committing; the operation is committed".into(),
                ));
            }
            Ok(out)
        })();
        self.inner
            .steps
            .fetch_add(steps.load(Ordering::Relaxed), Ordering::Relaxed);
        // Cleanup runs with no step limit, so an exhausted budget cannot leave the
        // transaction open: a rollback the limit interrupted is finished here.
        let reset = conn
            .progress_handler(0, None::<fn() -> bool>)
            .and_then(|_| {
                if conn.is_autocommit() {
                    Ok(())
                } else {
                    conn.execute_batch("ROLLBACK")
                }
            })
            .and_then(|_| conn.pragma_update(None, "synchronous", "NORMAL"))
            .and_then(|_| {
                conn.set_limit(Limit::SQLITE_LIMIT_LENGTH, previous_limit)
                    .map(|_| ())
            })
            .and_then(|_| conn.busy_timeout(self.inner.busy_timeout))
            .and_then(|_| match self.inner.deadline {
                Some(end) => install_operation_budget(&conn, end, self.inner.steps.clone())
                    .map_err(|_| rusqlite::Error::InvalidQuery),
                None => Ok(()),
            });
        if reset.is_err() {
            self.inner.identity_uncertain.store(true, Ordering::Release);
            return Err(Error::IdentityScanBudgetExceeded(
                "identity connection state uncertain".into(),
            ));
        }
        match result {
            Err(Error::Db(_)) if Instant::now() >= deadline => {
                Err(Error::IdentityScanBudgetExceeded("deadline".into()))
            }
            Err(Error::Db(rusqlite::Error::SqliteFailure(code, _)))
                if code.code == rusqlite::ErrorCode::OperationInterrupted
                    && steps.load(Ordering::Relaxed) >= step_limit =>
            {
                Err(Error::IdentityScanBudgetExceeded(
                    "SQLite step limit".into(),
                ))
            }
            Err(Error::Db(rusqlite::Error::SqliteFailure(code, _)))
                if matches!(
                    code.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) =>
            {
                Err(Error::IdentityScanBudgetExceeded(
                    "database busy: another writer held the lock".into(),
                ))
            }
            Err(Error::Db(rusqlite::Error::SqliteFailure(code, _)))
                if code.code == rusqlite::ErrorCode::TooBig =>
            {
                Err(Error::IdentityScanBudgetExceeded(
                    "SQLite value size limit".into(),
                ))
            }
            Err(Error::Db(rusqlite::Error::SqliteFailure(code, _)))
                if code.code == rusqlite::ErrorCode::OperationInterrupted =>
            {
                Err(Error::IdentityScanBudgetExceeded(
                    "SQLite scan limit or interruption".into(),
                ))
            }
            Err(Error::Db(_))
                if Instant::now() >= deadline || steps.load(Ordering::Relaxed) >= step_limit =>
            {
                Err(Error::IdentityScanBudgetExceeded(
                    "deadline or SQLite step limit".into(),
                ))
            }
            Err(error @ Error::Db(_)) => Err(identity_open_error(error)),
            other => other,
        }
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

fn load_existing_local_node(conn: &Connection) -> Result<(String, ed25519_dalek::SigningKey)> {
    let mut stmt =
        conn.prepare("SELECT node_id,secret_key,public_key FROM nodes WHERE is_local=1 LIMIT 2")?;
    let mut rows = stmt.query([])?;
    let row = rows
        .next()?
        .ok_or_else(|| Error::Invalid("local node missing; preserve and inspect home".into()))?;
    let node: String = row.get(0)?;
    let secret: Option<String> = row.get(1)?;
    let public: Option<String> = row.get(2)?;
    let key =
        crate::crypto::decode_secret(secret.as_deref().ok_or_else(|| {
            Error::Invalid("node key missing; preserve and inspect home".into())
        })?)?;
    if public.as_deref() != Some(crate::crypto::public_of(&key).as_str()) || rows.next()?.is_some()
    {
        return Err(Error::Invalid(
            "local node key is inconsistent; preserve and inspect home".into(),
        ));
    }
    Ok((node, key))
}

/// A write transaction plus the context every mutation needs.
pub(crate) struct Tx<'a> {
    pub tx: rusqlite::Transaction<'a>,
    pub node_id: &'a str,
    pub now: i64,
    pub key: &'a ed25519_dalek::SigningKey,
    /// Set by a transaction that creates a session binding: the new agent id, which names
    /// the operation in its durable fence. See [`Store::reconcile_identity_operations`].
    pub identity_op: std::cell::RefCell<Option<String>>,
}

/// An identity operation settled by [`Store::reconcile_identity_operations`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciledOperation {
    /// The agent id the operation was creating.
    pub operation: String,
    /// Whether its transaction is present (`true`) or was rolled back (`false`).
    pub committed: bool,
}

/// Directory, inside the Another home, of fences for identity commits in flight.
pub const FENCE_DIR: &str = "identity-ops";

/// More pending fences than this is not a state this build produces; writers refuse.
const MAX_FENCES: usize = 16;

/// SQLite VM steps one bounded operation may use in total.
pub const OPERATION_STEP_LIMIT: usize = 2_000_000;

/// Stop every statement on `conn` at `deadline` or once `steps` reaches the limit.
fn install_operation_budget(
    conn: &Connection,
    deadline: std::time::Instant,
    steps: Arc<std::sync::atomic::AtomicUsize>,
) -> Result<()> {
    // Checked on every VM step, so even a statement begun after the deadline stops at once.
    conn.progress_handler(
        1,
        Some(move || {
            steps.fetch_add(1, Ordering::Relaxed) + 1 >= OPERATION_STEP_LIMIT
                || std::time::Instant::now() >= deadline
        }),
    )?;
    Ok(())
}

fn fence_id_ok(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn fence_error(what: &str) -> Error {
    Error::IdentityUnavailable(format!(
        "identity operation fence: {what}; writes are refused until `{FENCE_DIR}` in the Another home is inspected"
    ))
}

/// Durably record that the transaction creating agent `operation` is about to commit.
/// Called inside that transaction, so no other writer can observe the fence before the
/// commit is attempted.
#[cfg(unix)]
fn publish_fence(db_path: &Path, operation: &str) -> Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let failed = |what: &str| {
        Error::IdentityUnavailable(format!(
            "cannot publish the identity operation fence ({what}); nothing was committed"
        ))
    };
    if !fence_id_ok(operation) {
        return Err(failed("invalid operation id"));
    }
    let home = db_path
        .parent()
        .ok_or_else(|| failed("no home directory"))?;
    let dir = home.join(FENCE_DIR);
    let created = match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(_) => return Err(failed("cannot create its directory")),
    };
    let path = dir.join(format!("{operation}.pending"));
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(b"session.binding.v1\n")?;
        file.sync_all()?;
        std::fs::File::open(&dir)?.sync_all()?;
        if created {
            std::fs::File::open(home)?.sync_all()?;
        }
        Ok(())
    };
    if write().is_err() {
        // Best effort: a fence left behind is settled as rolled back by the next writer.
        let _ = std::fs::remove_file(&path);
        return Err(failed("cannot write it durably"));
    }
    Ok(path)
}

#[cfg(not(unix))]
fn publish_fence(_db_path: &Path, _operation: &str) -> Result<PathBuf> {
    Err(Error::IdentityUnavailable(
        "identity operation fences are unsupported on this platform; nothing was committed".into(),
    ))
}

/// Remove the fence of a confirmed commit. Best effort: if it stays, the next writer finds
/// the operation committed and removes it.
fn clear_fence(path: &Path) {
    if std::fs::remove_file(path).is_ok() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
        }
    }
}

/// Settle every pending identity fence. `conn` must hold the database write lock: the
/// fence is published inside its transaction, so once another connection holds the lock
/// that transaction has ended, and the agent row it created is either visible (committed)
/// or will never appear (rolled back). Anything unreadable or unrecognized refuses.
fn reconcile_fences(conn: &Connection, db_path: Option<&Path>) -> Result<Vec<ReconciledOperation>> {
    let Some(home) = db_path.and_then(Path::parent) else {
        return Ok(Vec::new());
    };
    let dir = home.join(FENCE_DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err(fence_error("its directory is unreadable")),
    };
    let mut pending = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| fence_error("its directory is unreadable"))?;
        if pending.len() == MAX_FENCES {
            return Err(fence_error("too many pending operations"));
        }
        let name = entry.file_name();
        let operation = name
            .to_str()
            .and_then(|n| n.strip_suffix(".pending"))
            .filter(|id| fence_id_ok(id))
            .ok_or_else(|| fence_error("unrecognized entry"))?
            .to_string();
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            return Err(fence_error("unrecognized entry"));
        }
        pending.push((operation, entry.path()));
    }
    let mut settled = Vec::new();
    for (operation, path) in pending {
        let committed: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM agents WHERE agent_id = ?1)",
            [&operation],
            |r| r.get(0),
        )?;
        std::fs::remove_file(&path)
            .map_err(|_| fence_error("a settled operation cannot be cleared"))?;
        tracing::warn!(
            operation = %operation,
            committed,
            "settled an identity operation whose commit was not confirmed"
        );
        settled.push(ReconciledOperation {
            operation,
            committed,
        });
    }
    if !settled.is_empty() {
        let _ = std::fs::File::open(&dir).and_then(|d| d.sync_all());
    }
    Ok(settled)
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

/// Validate the entire catalog and its structural metadata against the checked-in v8 profile.
/// The candidate was extracted from an isolated home created by the 0403dce executable;
/// the drift test checks it against the current bundled migrations. Unknown profiles refuse.
fn verify_v8_schema(conn: &Connection, deadline: Option<std::time::Instant>) -> Result<()> {
    let result = verify_v8_schema_inner(conn, deadline);
    check_schema_deadline(deadline)?;
    result
}

fn identity_open_error(error: Error) -> Error {
    match error {
        Error::IdentityScanBudgetExceeded(_) | Error::IdentityUnavailable(_) => error,
        other => Error::IdentityUnavailable(other.to_string()),
    }
}

fn check_schema_deadline(deadline: Option<std::time::Instant>) -> Result<()> {
    if deadline.is_some_and(|end| std::time::Instant::now() >= end) {
        return Err(Error::IdentityScanBudgetExceeded("deadline".into()));
    }
    Ok(())
}

fn verify_v8_schema_inner(conn: &Connection, deadline: Option<std::time::Instant>) -> Result<()> {
    use rusqlite::types::ValueRef;
    let invalid = || Error::IdentityScanBudgetExceeded("unsupported database schema".into());
    check_schema_deadline(deadline)?;
    let expected: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("v8_schema_manifest.json")).map_err(|_| invalid())?;
    check_schema_deadline(deadline)?;
    if expected.len() > 256 {
        return Err(invalid());
    }
    let mut stmt = conn.prepare("SELECT type,name,tbl_name,CASE WHEN length(CAST(sql AS BLOB)) <= 16384 THEN sql END, length(CAST(sql AS BLOB)) FROM sqlite_schema ORDER BY type,name LIMIT 257")?;
    check_schema_deadline(deadline)?;
    let mut rows = stmt.query([])?;
    check_schema_deadline(deadline)?;
    let mut total = 0usize;
    let mut count = 0usize;
    loop {
        check_schema_deadline(deadline)?;
        let next = rows.next()?;
        check_schema_deadline(deadline)?;
        let Some(row) = next else { break };
        let Some(item) = expected.get(count) else {
            return Err(invalid());
        };
        count += 1;
        for (i, key) in [(0, "type"), (1, "name"), (2, "table")] {
            let ValueRef::Text(raw) = row.get_ref(i)? else {
                return Err(invalid());
            };
            if raw.len() > 1024 || item[key].as_str() != std::str::from_utf8(raw).ok() {
                return Err(invalid());
            }
            total = total.checked_add(raw.len()).ok_or_else(invalid)?;
        }
        match (row.get_ref(3)?, row.get_ref(4)?) {
            (ValueRef::Null, ValueRef::Null) if item["sql"].is_null() => {}
            (ValueRef::Text(raw), ValueRef::Integer(len))
                if (0..=16384).contains(&len) && raw.len() == len as usize =>
            {
                total = total.checked_add(raw.len()).ok_or_else(invalid)?;
                if item["sql"].as_str() != std::str::from_utf8(raw).ok() {
                    return Err(invalid());
                }
            }
            _ => return Err(invalid()),
        }
        if total > 1024 * 1024 {
            return Err(invalid());
        }
        check_schema_deadline(deadline)?;
    }
    if count != expected.len() {
        return Err(invalid());
    }
    drop(rows);
    drop(stmt);

    for item in &expected {
        let kind = item["type"].as_str().ok_or_else(invalid)?;
        let name = item["name"].as_str().ok_or_else(invalid)?;
        let probes: &[(&str, &str)] = match kind {
            "table" => &[
                ("columns", "SELECT cid,name,type,\"notnull\",dflt_value,pk,hidden FROM pragma_table_xinfo(?1) LIMIT ?2"),
                ("foreign_keys", "SELECT id,seq,\"table\",\"from\",\"to\",on_update,on_delete,match FROM pragma_foreign_key_list(?1) LIMIT ?2"),
                ("indexes", "SELECT seq,name,\"unique\",origin,partial FROM pragma_index_list(?1) LIMIT ?2"),
            ],
            "index" => &[("index_columns", "SELECT seqno,cid,name,\"desc\",coll,key FROM pragma_index_xinfo(?1) LIMIT ?2")],
            _ => &[],
        };
        for (key, sql) in probes {
            check_schema_deadline(deadline)?;
            let expected_rows = item[*key].as_array().ok_or_else(invalid)?;
            let mut query = conn.prepare(sql)?;
            check_schema_deadline(deadline)?;
            let column_count = query.column_count();
            let mut rows = query.query(params![name, (expected_rows.len() + 1) as i64])?;
            check_schema_deadline(deadline)?;
            let mut index = 0usize;
            loop {
                check_schema_deadline(deadline)?;
                let next = rows.next()?;
                check_schema_deadline(deadline)?;
                let Some(row) = next else { break };
                let wanted = expected_rows
                    .get(index)
                    .and_then(|v| v.as_array())
                    .ok_or_else(invalid)?;
                if wanted.len() != column_count {
                    return Err(invalid());
                }
                for (column, expected_value) in wanted.iter().enumerate() {
                    let value = match row.get_ref(column)? {
                        ValueRef::Null => serde_json::Value::Null,
                        ValueRef::Integer(value) => serde_json::Value::from(value),
                        ValueRef::Text(raw) if raw.len() <= 1024 => serde_json::Value::from(
                            std::str::from_utf8(raw).map_err(|_| invalid())?,
                        ),
                        _ => return Err(invalid()),
                    };
                    if &value != expected_value {
                        return Err(invalid());
                    }
                }
                check_schema_deadline(deadline)?;
                index += 1;
            }
            if index != expected_rows.len() {
                return Err(invalid());
            }
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
            identity_op: std::cell::RefCell::new(None),
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
    fn schema_interruption_preserves_sqlite_cause_for_step_classification() {
        let db = Connection::open_in_memory().unwrap();
        for migration in MIGRATIONS {
            db.execute_batch(migration).unwrap();
        }
        db.progress_handler(1, Some(|| true)).unwrap();
        let result = verify_v8_schema(&db, None);
        db.progress_handler(0, None::<fn() -> bool>).unwrap();
        assert!(
            matches!(result, Err(Error::Db(rusqlite::Error::SqliteFailure(code, _))) if code.code == rusqlite::ErrorCode::OperationInterrupted)
        );
    }

    #[test]
    fn interrupted_schema_read_is_typed_and_keeps_sqlite_cause() {
        let db = Connection::open_in_memory().unwrap();
        for migration in MIGRATIONS {
            db.execute_batch(migration).unwrap();
        }
        db.progress_handler(1, Some(|| true)).unwrap();
        let result = verify_v8_schema(&db, None).map_err(identity_open_error);
        db.progress_handler(0, None::<fn() -> bool>).unwrap();
        assert!(
            matches!(result, Err(Error::IdentityUnavailable(ref reason)) if reason.to_lowercase().contains("interrupt")),
            "{result:?}"
        );
    }

    #[test]
    fn identity_schema_step_exhaustion_is_classified_before_closure() {
        let store = Store::open_in_memory(Clock::System).unwrap();
        let result = store.write_identity_with_step_limit(100, |_, _| -> Result<()> {
            panic!("identity mutation ran after schema step exhaustion")
        });
        assert!(
            matches!(result, Err(Error::IdentityScanBudgetExceeded(ref reason)) if reason == "SQLite step limit"),
            "{result:?}"
        );
    }

    #[test]
    fn checked_in_v8_schema_matches_current_migrations() {
        let db = Connection::open_in_memory().unwrap();
        for migration in MIGRATIONS {
            db.execute_batch(migration).unwrap();
        }
        verify_v8_schema(&db, None).unwrap();
        let partial: i64 = db.query_row(
            "SELECT partial FROM pragma_index_list('resources') WHERE name = 'resources_live_identity'",
            [],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(partial, 1);
    }

    #[test]
    fn schema_validation_refuses_an_elapsed_deadline() {
        let db = Connection::open_in_memory().unwrap();
        for migration in MIGRATIONS {
            db.execute_batch(migration).unwrap();
        }
        assert!(matches!(
            verify_v8_schema(&db, Some(std::time::Instant::now())),
            Err(Error::IdentityScanBudgetExceeded(ref reason)) if reason == "deadline"
        ));
    }

    #[test]
    fn schema_profile_refuses_unknown_tables_and_triggers() {
        for extra in [
            "CREATE TABLE extra_data(value TEXT)",
            "CREATE TRIGGER unknown_trigger AFTER INSERT ON nodes BEGIN SELECT 1; END",
        ] {
            let db = Connection::open_in_memory().unwrap();
            for migration in MIGRATIONS {
                db.execute_batch(migration).unwrap();
            }
            db.execute_batch(extra).unwrap();
            assert!(verify_v8_schema(&db, None).is_err());
        }
    }

    #[test]
    fn noncanonical_equivalent_identity_index_is_refused() {
        let home = tempfile::tempdir().unwrap();
        drop(Store::open(home.path()).unwrap());
        let db = Connection::open(home.path().join(DB_FILE)).unwrap();
        db.execute_batch("DROP INDEX agents_identity; CREATE UNIQUE INDEX agents_identity ON agents (node_id, name, IFNULL(project_id, ''))").unwrap();
        assert!(Store::open_quick(home.path(), std::time::Duration::from_millis(50)).is_err());
    }

    #[test]
    fn damaged_required_indexes_are_refused_without_repair() {
        for replacement in [
            "",
            "CREATE INDEX agents_identity ON agents(node_id, name, project_id)",
            "CREATE UNIQUE INDEX agents_identity ON agents(node_id, name) WHERE project_id IS NOT NULL",
        ] {
            let home = tempfile::tempdir().unwrap();
            drop(Store::open(home.path()).unwrap());
            let db = Connection::open(home.path().join(DB_FILE)).unwrap();
            db.execute_batch("DROP INDEX agents_identity").unwrap();
            if !replacement.is_empty() {
                db.execute_batch(replacement).unwrap();
            }
            let before: (i64, i64) = db.query_row(
                "SELECT (SELECT count(*) FROM agents), (SELECT count(*) FROM events)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).unwrap();
            assert!(Store::open(home.path()).is_err());
            assert!(Store::open_quick(home.path(), std::time::Duration::from_millis(50)).is_err());
            let after: (i64, i64) = db.query_row(
                "SELECT (SELECT count(*) FROM agents), (SELECT count(*) FROM events)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).unwrap();
            assert_eq!(before, after);
        }
    }

    #[test]
    fn uncertain_identity_connection_reopens_before_resolving_again() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let store = Store::open(&home).unwrap();
        let root = project.to_str().unwrap();
        let actor = store
            .session_actor(
                "claude",
                crate::identity::Program::Claude,
                root,
                None,
                "fixture",
                true,
            )
            .unwrap()
            .0;
        store
            .inner
            .identity_uncertain
            .store(true, Ordering::Release);
        assert!(store.events(10).is_err());
        let resolved = store
            .session_actor(
                "claude",
                crate::identity::Program::Claude,
                root,
                None,
                "fixture",
                false,
            )
            .unwrap()
            .0;
        assert_eq!(resolved, actor);
        assert!(store.events(10).is_ok());
    }

    #[test]
    fn uncertain_identity_commit_reconciles_both_simulated_outcomes() {
        for fault in [1, 2] {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            let project = dir.path().join("project");
            std::fs::create_dir_all(&project).unwrap();
            let store = Store::open(&home).unwrap();
            let root = project.to_str().unwrap();
            store.ensure_project("fixture", root).unwrap();
            store
                .inner
                .identity_commit_fault
                .store(fault, Ordering::Release);
            let first = store.session_actor(
                "claude",
                crate::identity::Program::Claude,
                root,
                None,
                "fixture",
                true,
            );
            assert!(matches!(first, Err(Error::IdentityScanBudgetExceeded(_))));
            assert!(store.events(10).is_err());
            let actor = store
                .session_actor(
                    "claude",
                    crate::identity::Program::Claude,
                    root,
                    None,
                    "fixture",
                    true,
                )
                .unwrap()
                .0;
            assert!(store.is_session_actor(&actor).unwrap());
            assert_eq!(store.agents().unwrap().len(), 1);
            assert_eq!(
                store
                    .events(100)
                    .unwrap()
                    .iter()
                    .filter(|e| e.kind == "session.binding.v1")
                    .count(),
                1
            );
        }
    }

    /// A binding commit whose outcome was not confirmed leaves a fence on disk that names
    /// the operation. A later process settles exactly that operation, whichever way the
    /// commit went, before it writes anything.
    #[test]
    fn unconfirmed_binding_commit_is_fenced_and_settled_across_restart() {
        for (fault, committed) in [(1, true), (2, false)] {
            let dir = tempfile::tempdir().unwrap();
            let home = dir.path().join("home");
            let project = dir.path().join("project");
            std::fs::create_dir_all(&project).unwrap();
            let root = project.to_str().unwrap();
            let store = Store::open(&home).unwrap();
            store.ensure_project("fixture", root).unwrap();
            store
                .inner
                .identity_commit_fault
                .store(fault, Ordering::Release);
            let first = store.session_actor(
                "claude",
                crate::identity::Program::Claude,
                root,
                None,
                "fixture",
                true,
            );
            assert!(
                matches!(first, Err(Error::IdentityScanBudgetExceeded(ref why)) if why.contains("uncertain")),
                "{first:?}"
            );
            let fences: Vec<String> = std::fs::read_dir(home.join(FENCE_DIR))
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            assert_eq!(fences.len(), 1, "{fences:?}");
            let operation = fences[0].strip_suffix(".pending").unwrap().to_owned();
            // The process that saw the failure is fenced until it reconciles.
            assert!(store.ensure_project("fixture", root).is_err());
            drop(store);

            // A new process: the fence is still there, and its first write settles it.
            let restarted =
                Store::open_quick(&home, std::time::Duration::from_millis(200)).unwrap();
            assert!(home.join(FENCE_DIR).join(&fences[0]).exists());
            let settled = restarted.reconcile_identity_operations().unwrap();
            assert_eq!(
                settled,
                [ReconciledOperation {
                    operation: operation.clone(),
                    committed
                }]
            );
            assert_eq!(std::fs::read_dir(home.join(FENCE_DIR)).unwrap().count(), 0);
            let agents = restarted.agents().unwrap();
            assert_eq!(agents.len(), usize::from(committed));
            assert!(agents.iter().all(|a| a.agent_id == operation));

            // The session then resolves to one actor and one binding, never two.
            let actor = restarted
                .session_actor(
                    "claude",
                    crate::identity::Program::Claude,
                    root,
                    None,
                    "fixture",
                    true,
                )
                .unwrap()
                .0;
            assert!(restarted.is_session_actor(&actor).unwrap());
            assert_eq!(restarted.agents().unwrap().len(), 1);
            assert_eq!(
                restarted
                    .events(100)
                    .unwrap()
                    .iter()
                    .filter(|e| e.kind == "session.binding.v1")
                    .count(),
                1
            );
            assert_eq!(std::fs::read_dir(home.join(FENCE_DIR)).unwrap().count(), 0);
        }
    }

    /// A fence that cannot be written durably stops the commit: nothing is created.
    #[test]
    fn a_binding_is_not_committed_when_its_fence_cannot_be_published() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let root = project.to_str().unwrap();
        let store = Store::open(&home).unwrap();
        store.ensure_project("fixture", root).unwrap();
        // A file where the fence directory must go.
        std::fs::write(home.join(FENCE_DIR), b"").unwrap();
        let result = store.session_actor(
            "claude",
            crate::identity::Program::Claude,
            root,
            None,
            "fixture",
            true,
        );
        assert!(
            matches!(result, Err(Error::IdentityUnavailable(ref why)) if why.contains("fence")),
            "{result:?}"
        );
        std::fs::remove_file(home.join(FENCE_DIR)).unwrap();
        assert!(store.agents().unwrap().is_empty());
        assert!(store
            .events(100)
            .unwrap()
            .iter()
            .all(|e| e.kind != "session.binding.v1"));
    }

    #[test]
    fn parent_sync_failure_prevents_source_migration() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let backup = dir.path().join("backup");
        drop(Store::open(&home).unwrap());
        let db = home.join(DB_FILE);
        let source = Connection::open(&db).unwrap();
        source.execute_batch("DROP TABLE announcements; PRAGMA user_version=7; CREATE TABLE preserved(v TEXT); INSERT INTO preserved VALUES ('unchanged');").unwrap();
        drop(source);
        let mut called = false;
        let result = Store::upgrade_offline_with_parent_sync(&home, &backup, true, |parent| {
            called = true;
            assert_eq!(parent, std::fs::canonicalize(dir.path()).unwrap());
            assert!(backup.join(DB_FILE).is_file());
            assert!(backup.join("backup.json").is_file());
            let source = Connection::open(&db).unwrap();
            assert_eq!(
                source
                    .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                    .unwrap(),
                7
            );
            Err(std::io::Error::other("injected sync failure"))
        });
        assert!(called);
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("source was not migrated"));
        let source = Connection::open(&db).unwrap();
        assert_eq!(
            source
                .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            7
        );
        assert_eq!(
            source
                .query_row("SELECT v FROM preserved", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "unchanged"
        );
    }

    #[test]
    fn parent_sync_finishes_before_source_migration() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let backup = dir.path().join("backup");
        drop(Store::open(&home).unwrap());
        let db = home.join(DB_FILE);
        let source = Connection::open(&db).unwrap();
        source
            .execute_batch("DROP TABLE announcements; PRAGMA user_version=7;")
            .unwrap();
        drop(source);
        let mut called = false;
        Store::upgrade_offline_with_parent_sync(&home, &backup, true, |parent| {
            called = true;
            assert!(backup.join(DB_FILE).is_file());
            assert!(backup.join("backup.json").is_file());
            let source = Connection::open(&db).unwrap();
            assert_eq!(
                source
                    .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                    .unwrap(),
                7
            );
            std::fs::File::open(parent)?.sync_all()
        })
        .unwrap();
        assert!(called);
        let source = Connection::open(&db).unwrap();
        assert_eq!(
            source
                .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            8
        );
    }

    #[test]
    fn intervening_write_refuses_upgrade_without_discarding_it() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        drop(Store::open(&home).unwrap());
        let db = home.join(DB_FILE);
        let source = Connection::open(&db).unwrap();
        source
            .execute_batch("DROP TABLE announcements; PRAGMA user_version=7;")
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
        let result = migrate_offline(&mut source, 7, version, &node);
        assert!(result.is_err());
        assert_eq!(
            source
                .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            7
        );
        assert_eq!(
            source
                .query_row("SELECT v FROM intervening", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "newer"
        );
    }
}
