//! Read-only hold check for the advisory pre-commit guard (`another hook pre-commit`).
//!
//! It answers "is any staged file under another agent's live hold?" with the edit guard's
//! matching, and changes no data: no sweep, no event, no project registration, no
//! repository-link cache refresh. The database is opened with SQLite's read-only flag
//! (SQLite may still create the `-wal` and `-shm` files next to it).
//!
//! The edit guard refreshes the repository links of projects that hold live holds before
//! it compares them (`link_unlinked`), so a sibling checkout whose link was never cached,
//! or was cached under older key rules, still counts. This module builds that same view in
//! memory: a cached link in the current format is used as stored, anything else is
//! recomputed from the project's files with `repo::link_for` and not written back.

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, OpenFlags, MAIN_DB};
use serde::Serialize;

use crate::error::{Error, Result};
use crate::holds::{folds_case, hold_from_row, overlap_on_volume, Hold, HOLD_SELECT_REPO};
use crate::identity::{find_project, resolve_agent, Project};
use crate::repo::{cached, in_repo, link_for, RepoLink};
use crate::store::DB_FILE;

/// The longest staged name checked, in bytes (PATH_MAX on Linux).
pub const MAX_NAME_BYTES: usize = 4096;

/// A read-only handle on this node's database.
pub struct CommitReader {
    conn: Connection,
    node_id: String,
}

/// One group of staged paths: the project directory they were grouped under (a registered
/// project's path, or the git top level) and their paths relative to it.
#[derive(Debug, Clone)]
pub struct CommitGroup {
    pub dir: String,
    pub paths: Vec<String>,
}

/// A staged path under someone else's live hold.
#[derive(Debug, Clone, Serialize)]
pub struct CommitConflict {
    /// The staged path, relative to its group's directory.
    pub path: String,
    /// The group's directory.
    pub dir: String,
    pub held: Hold,
}

/// A staged path that was not checked, and why.
#[derive(Debug, Clone, Serialize)]
pub struct CommitSkip {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CommitCheck {
    /// The committer's address when it resolved to an agent of this node; `None` means
    /// every overlapping hold was reported, possibly including the committer's own.
    pub agent: Option<String>,
    /// Why a given `--agent` was not used, when it was not.
    pub agent_note: Option<String>,
    pub checked: usize,
    pub skipped: Vec<CommitSkip>,
    pub conflicts: Vec<CommitConflict>,
    /// False when the time budget ran out before every candidate project had a link, or
    /// before every group was checked.
    pub links_complete: bool,
}

/// Whether `e` is SQLite reporting a lock it waited for in vain (busy or locked).
pub fn is_busy_error(e: &Error) -> bool {
    use rusqlite::ErrorCode;
    matches!(
        e,
        Error::Db(rusqlite::Error::SqliteFailure(f, _))
            if matches!(f.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

/// Whether `e` is SQLite reporting a statement interrupted by the deadline watchdog.
fn is_interrupted(e: &Error) -> bool {
    matches!(
        e,
        Error::Db(rusqlite::Error::SqliteFailure(f, _))
            if f.code == rusqlite::ErrorCode::OperationInterrupted
    )
}

/// Mark the check incomplete for the time budget: the path at `at` and every one after it
/// is skipped. Conflicts and skips already recorded stay.
fn budget_ran_out(out: &mut CommitCheck, groups: &[CommitGroup], at: (usize, usize)) {
    out.links_complete = false;
    let (gi, pi) = at;
    for (i, group) in groups.iter().enumerate().skip(gi) {
        let from = if i == gi { pi } else { 0 };
        for p in group.paths.iter().skip(from) {
            let path = if p.len() > MAX_NAME_BYTES {
                p.chars().take(200).collect()
            } else {
                p.clone()
            };
            out.skipped.push(CommitSkip {
                path,
                reason: "time budget".into(),
            });
        }
    }
}

/// How often the watchdog interrupts again once the deadline has passed.
const REINTERRUPT: Duration = Duration::from_millis(5);

/// Interrupts the connection from its own thread once the deadline has passed, and again
/// every [`REINTERRUPT`] until it is dropped. One interrupt is not enough: SQLite drops an
/// interrupt that arrives while no statement runs, so a statement that starts later would
/// run unbounded. Dropping the watchdog stops the thread at once.
struct Watchdog {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watchdog {
    /// `None` when no thread could be started; the caller must then not query at all.
    fn start(handle: rusqlite::InterruptHandle, deadline: Instant) -> Option<Watchdog> {
        let (stop, rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("another-commit-deadline".into())
            .spawn(move || {
                let mut wait = deadline.saturating_duration_since(Instant::now());
                while let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(wait) {
                    handle.interrupt();
                    wait = REINTERRUPT;
                }
            })
            .ok()?;
        Some(Watchdog {
            stop: Some(stop),
            thread: Some(thread),
        })
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        // Closing the channel wakes the thread at once.
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A glob metacharacter in a staged name: the name is never treated as a pattern.
fn has_glob_char(path: &str) -> bool {
    path.contains(['*', '?', '[', '{'])
}

impl CommitReader {
    /// Open `home`'s database read-only. Nothing is created, migrated or written; `busy`
    /// bounds every wait for a lock. Fails when the database is missing, its schema is not
    /// the one this build expects, or the read-only open itself fails (it is never retried
    /// read-write).
    pub fn open(home: &Path, busy: Duration) -> Result<CommitReader> {
        let path = home.join(DB_FILE);
        if !path.is_file() {
            return Err(Error::NotFound(format!(
                "no database at {}; run `another init`",
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
        if !conn.is_readonly(MAIN_DB)? {
            return Err(Error::Invalid("the database did not open read-only".into()));
        }
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let want = crate::store::MIGRATIONS.len() as i64;
        if version != want {
            return Err(Error::Invalid(format!(
                "database schema is v{version}, this build expects v{want}"
            )));
        }
        let node_id: String =
            conn.query_row("SELECT node_id FROM nodes WHERE is_local = 1", [], |r| {
                r.get(0)
            })?;
        Ok(CommitReader { conn, node_id })
    }

    /// Whether the connection is read-only (always true for a handle from [`Self::open`]).
    pub fn is_readonly(&self) -> bool {
        self.conn.is_readonly(MAIN_DB).unwrap_or(false)
    }

    /// This node's registered project directories, or `None` when `deadline` passes first
    /// (or no deadline watchdog could be started).
    pub fn project_dirs(&self, deadline: Instant) -> Result<Option<Vec<String>>> {
        let Some(_watchdog) = self.arm(deadline)? else {
            return Ok(None);
        };
        let read = || -> Result<Vec<String>> {
            let mut stmt = self
                .conn
                .prepare("SELECT abs_path FROM projects WHERE node_id = ?1 ORDER BY abs_path")?;
            let rows = stmt.query_map([&self.node_id], |r| r.get(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<String>>>()?)
        };
        match read() {
            Ok(dirs) => Ok(Some(dirs)),
            Err(e) if is_interrupted(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Bound this connection's work by `deadline`: lock waits get the time left, and a
    /// watchdog interrupts statements once it has passed. `None` when the deadline has
    /// already passed or no watchdog could be started; then nothing may be queried.
    fn arm(&self, deadline: Instant) -> Result<Option<Watchdog>> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        self.conn.busy_timeout(left)?;
        Ok(Watchdog::start(self.conn.get_interrupt_handle(), deadline))
    }

    /// Check `groups` against live holds. `agent` (`name` or `name@project`) is the
    /// committer: its own holds are left out when it resolves to an agent of this node.
    ///
    /// Work stops at `deadline`: the path and hold loops check it, a lock wait is capped to
    /// the time left, and a statement still running at the deadline is interrupted. The
    /// result then has `links_complete` false, every path not fully checked skipped for
    /// "time budget", and the conflicts already found. Reading a project's files to work out
    /// its repository link is not interrupted; the deadline is checked between projects.
    pub fn check(
        &self,
        agent: Option<&str>,
        groups: &[CommitGroup],
        deadline: Instant,
    ) -> Result<CommitCheck> {
        let mut out = CommitCheck {
            links_complete: true,
            ..CommitCheck::default()
        };
        // No deadline or no watchdog: no query runs, and every path is reported unchecked.
        let Some(_watchdog) = self.arm(deadline)? else {
            budget_ran_out(&mut out, groups, (0, 0));
            return Ok(out);
        };
        let mut at = (0, 0);
        match self.check_into(&mut out, agent, groups, deadline, &mut at) {
            Ok(true) => {}
            Ok(false) => budget_ran_out(&mut out, groups, at),
            Err(e) if is_interrupted(&e) => budget_ran_out(&mut out, groups, at),
            Err(e) => return Err(e),
        }
        Ok(out)
    }

    /// The body of [`Self::check`]. `at` is the group and path being worked on; `Ok(false)`
    /// means the deadline passed there, before that path was fully checked.
    fn check_into(
        &self,
        out: &mut CommitCheck,
        agent: Option<&str>,
        groups: &[CommitGroup],
        deadline: Instant,
        at: &mut (usize, usize),
    ) -> Result<bool> {
        // Every query below starts only while time is left (review IMPL-R2-1); one that is
        // still running at the deadline is interrupted by the watchdog.
        let expired = || Instant::now() >= deadline;
        let agent_id = match agent {
            None => None,
            Some(who) => match resolve_agent(&self.conn, &self.node_id, who) {
                Ok(a) => {
                    out.agent = Some(a.address.clone());
                    Some(a.id)
                }
                Err(e) if is_interrupted(&e) => return Err(e),
                Err(e) => {
                    out.agent_note = Some(format!("{who}: {e}"));
                    None
                }
            },
        };
        let now = crate::time::now_ms();
        if expired() {
            return Ok(false);
        }
        let (links, complete) = self.effective_links(now, deadline)?;
        if !complete || expired() {
            return Ok(false);
        }
        let holds = self.live_holds(now, agent_id.as_deref())?;

        for (gi, group) in groups.iter().enumerate() {
            *at = (gi, 0);
            if Instant::now() >= deadline {
                return Ok(false);
            }
            let project = match find_project(&self.conn, &self.node_id, &group.dir) {
                Ok(p) => p,
                Err(Error::NotFound(_)) => {
                    for p in &group.paths {
                        out.skipped.push(CommitSkip {
                            path: p.clone(),
                            reason: "not in a registered project".into(),
                        });
                    }
                    continue;
                }
                Err(e) => return Err(e),
            };
            let mine = match links.get(&project.project_id) {
                Some(l) => l.clone(),
                None if expired() => return Ok(false),
                None => self.link_of(&project)?,
            };
            let (my_key, my_prefix) =
                mine.map_or((String::new(), String::new()), |l| (l.key, l.prefix));
            let fold = folds_case(Path::new(&project.abs_path));
            for (pi, raw) in group.paths.iter().enumerate() {
                *at = (gi, pi);
                if Instant::now() >= deadline {
                    return Ok(false);
                }
                // An empty name would normalize to the whole project and match every hold.
                if raw.is_empty() {
                    out.skipped.push(CommitSkip {
                        path: raw.clone(),
                        reason: "empty name".into(),
                    });
                    continue;
                }
                // Longer than any path a file system accepts; compiling it as a pattern
                // would also be slow (globset panics on a multi-megabyte name).
                if raw.len() > MAX_NAME_BYTES {
                    out.skipped.push(CommitSkip {
                        path: raw.chars().take(200).collect(),
                        reason: "name too long".into(),
                    });
                    continue;
                }
                if has_glob_char(raw) {
                    out.skipped.push(CommitSkip {
                        path: raw.clone(),
                        reason: "glob character in the name".into(),
                    });
                    continue;
                }
                let pattern = match crate::holds::normalize_pattern(&project, raw) {
                    Ok(p) => p,
                    Err(e) => {
                        out.skipped.push(CommitSkip {
                            path: raw.clone(),
                            reason: format!("cannot be checked: {e}"),
                        });
                        continue;
                    }
                };
                out.checked += 1;
                for (held, held_project) in &holds {
                    // Matching a glob is the slow part: stop between holds, and count this
                    // path as not checked (conflicts it already showed are kept).
                    if Instant::now() >= deadline {
                        out.checked -= 1;
                        return Ok(false);
                    }
                    let overlap = if *held_project == project.project_id {
                        overlap_on_volume(&pattern, &held.pattern, fold)
                    } else {
                        // A sibling checkout of the same repository, by its effective link.
                        let Some(Some(theirs)) = links.get(held_project) else {
                            continue;
                        };
                        if my_key.is_empty() || theirs.key != my_key {
                            continue;
                        }
                        overlap_on_volume(
                            &in_repo(&my_prefix, &pattern),
                            &in_repo(&theirs.prefix, &held.pattern),
                            fold,
                        )
                    };
                    if overlap {
                        out.conflicts.push(CommitConflict {
                            path: raw.clone(),
                            dir: group.dir.clone(),
                            held: held.clone(),
                        });
                    }
                }
            }
        }
        Ok(true)
    }

    /// Live holds on this node's projects, without the committer's own, with the id of the
    /// project each is in.
    fn live_holds(&self, now: i64, agent_id: Option<&str>) -> Result<Vec<(Hold, String)>> {
        let mut stmt = self.conn.prepare(&format!(
            "{HOLD_SELECT_REPO} WHERE h.released_at IS NULL AND h.expires_at > ?1
               AND p.node_id = ?2 AND (?3 IS NULL OR h.agent_id != ?3)
             ORDER BY h.created_at, h.hold_id"
        ))?;
        let rows = stmt.query_map(params![now, self.node_id, agent_id], |r| {
            Ok((hold_from_row(r)?, r.get::<_, String>(10)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The edit guard's repository links for every local project with a live hold, in
    /// memory: the cached link when it is in the current format, else recomputed from the
    /// project's files. Returns false as the second value when `deadline` passed first.
    #[allow(clippy::type_complexity)]
    fn effective_links(
        &self,
        now: i64,
        deadline: Instant,
    ) -> Result<(HashMap<String, Option<RepoLink>>, bool)> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT p.project_id, p.node_id, p.abs_path, p.slug, p.created_at
             FROM holds h JOIN projects p ON p.project_id = h.project_id
             WHERE h.released_at IS NULL AND h.expires_at > ?1 AND p.node_id = ?2",
        )?;
        let projects = stmt
            .query_map(params![now, self.node_id], |r| {
                Ok(Project {
                    project_id: r.get(0)?,
                    node_id: r.get(1)?,
                    abs_path: r.get(2)?,
                    slug: r.get(3)?,
                    created_at: crate::time::fmt_ts(r.get(4)?),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut links = HashMap::new();
        for p in &projects {
            if Instant::now() >= deadline {
                return Ok((links, false));
            }
            match cached(&self.conn, &p.project_id)? {
                Some(link) => {
                    links.insert(p.project_id.clone(), link);
                }
                None => {
                    links.insert(p.project_id.clone(), link_for(Path::new(&p.abs_path)));
                }
            }
        }
        Ok((links, true))
    }

    /// `repo::link_of` without the cache write.
    fn link_of(&self, project: &Project) -> Result<Option<RepoLink>> {
        Ok(match cached(&self.conn, &project.project_id)? {
            Some(link) => link,
            None => link_for(Path::new(&project.abs_path)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AcquireHolds, Program, RegisterAgent, Store};
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// When each statement on a traced connection started, and its SQL. Only
    /// `no_query_starts_after_the_deadline` traces, so the tests do not share entries.
    static STARTED: Mutex<Vec<(Instant, String)>> = Mutex::new(Vec::new());

    fn record(event: rusqlite::trace::TraceEvent<'_>) {
        if let rusqlite::trace::TraceEvent::Stmt(stmt, _) = event {
            STARTED
                .lock()
                .unwrap()
                .push((Instant::now(), stmt.sql().into_owned()));
        }
    }

    /// A home with `n` projects whose repository links are not cached and whose git
    /// configs are large (about 240 KB, under the 256 KiB read cap), so working out
    /// their links takes time. Each has a live hold.
    fn slow_links(n: usize) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let home = root.join("home");
        let store = Store::open(&home).unwrap();
        let config: String = std::iter::once("[core]\n".to_string())
            .chain((0..60_000).map(|i| format!("\tk{i} = v\n")))
            .collect();
        let register = |name: &str, project: &Path| {
            store
                .register_agent(RegisterAgent {
                    name: name.into(),
                    program: Program::Claude,
                    project: Some(project.to_string_lossy().into_owned()),
                    model: None,
                    description: None,
                })
                .unwrap();
        };
        let main = root.join("main");
        std::fs::create_dir_all(&main).unwrap();
        register("alice", &main);
        for i in 0..n {
            let p = root.join(format!("p{i}"));
            std::fs::create_dir_all(p.join(".git")).unwrap();
            std::fs::write(p.join(".git/config"), &config).unwrap();
            register(&format!("b{i}"), &p);
            store
                .acquire_holds(AcquireHolds {
                    agent: format!("b{i}"),
                    project: p.to_string_lossy().into_owned(),
                    paths: vec!["src/x.rs".into()],
                    ttl_seconds: Some(3600),
                    exclusive: true,
                    reason: None,
                    repo_wide: false,
                })
                .unwrap();
        }
        drop(store);
        let db = Connection::open(home.join(DB_FILE)).unwrap();
        db.execute("DELETE FROM project_repos", []).unwrap();
        (dir, home, main)
    }

    /// Review IMPL-R2-1. SQLite forgets an interrupt that arrives between statements, so a
    /// query started after the deadline used to run unbounded: the live-hold query ran
    /// after the link work had used up the budget. Now no query starts once the deadline
    /// has passed, whether it passed before the check or during the link work.
    #[test]
    fn no_query_starts_after_the_deadline() {
        // 40 configs of 240 KB: well over the 1 ms budget below to work out on any machine
        // (the assertion on `links` makes sure the budget did run out during that work).
        let (_dir, home, main) = slow_links(40);
        let groups = [CommitGroup {
            dir: main.to_string_lossy().into_owned(),
            paths: vec!["src/x.rs".into(), "src/y.rs".into()],
        }];
        for budget in [None, Some(Duration::from_millis(1))] {
            let reader = CommitReader::open(&home, Duration::from_millis(300)).unwrap();
            let stmt = rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT;
            reader.conn.trace_v2(stmt, Some(record));
            STARTED.lock().unwrap().clear();
            let deadline = match budget {
                // Already spent when the check starts.
                None => Instant::now(),
                Some(b) => Instant::now() + b,
            };
            let check = reader.check(Some("alice"), &groups, deadline).unwrap();
            reader.conn.trace_v2(stmt, None);
            let started = std::mem::take(&mut *STARTED.lock().unwrap());
            let late: Vec<&String> = started
                .iter()
                .filter(|(t, _)| *t > deadline)
                .map(|(_, sql)| sql)
                .collect();
            assert!(late.is_empty(), "{budget:?}: started late: {late:?}");
            if budget.is_none() {
                assert!(started.is_empty(), "{started:?}");
            } else {
                // The case under test: the deadline passed during the link work, which
                // read some of the cached-link rows (one query per project) but not all.
                let links = started
                    .iter()
                    .filter(|(_, sql)| sql.contains("project_repos"))
                    .count();
                assert!(links > 0 && links < 40, "{links} link lookups");
            }
            assert!(!check.links_complete, "{budget:?}");
            assert_eq!(check.checked + check.skipped.len(), 2, "{check:?}");
        }
    }

    /// The watchdog keeps interrupting after the deadline, so a statement that starts late
    /// (here on purpose) is still stopped.
    #[test]
    fn a_statement_started_after_the_deadline_is_interrupted() {
        let conn = Connection::open_in_memory().unwrap();
        let deadline = Instant::now() + Duration::from_millis(5);
        let _w = Watchdog::start(conn.get_interrupt_handle(), deadline).unwrap();
        // Let the first interrupt land while nothing runs.
        std::thread::sleep(Duration::from_millis(30));
        let started = Instant::now();
        let r: rusqlite::Result<i64> = conn.query_row(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c)
             SELECT count(*) FROM c",
            [],
            |r| r.get(0),
        );
        let err = Error::Db(r.unwrap_err());
        assert!(is_interrupted(&err), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
