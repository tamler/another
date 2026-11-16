//! Read-only hold check for the advisory pre-commit guard (`another hook pre-commit`).
//!
//! It answers "is any staged file under another agent's live hold?" with the edit guard's
//! matching, and writes nothing: no sweep, no event, no project registration, no
//! repository-link cache refresh. The database is opened with SQLite's read-only flag.
//!
//! The edit guard refreshes the repository links of projects that hold live holds before
//! it compares them (`link_unlinked`), so a sibling checkout whose link was never cached,
//! or was cached under older key rules, still counts. This module builds that same view in
//! memory: a cached link in the current format is used as stored, anything else is
//! recomputed from the project's files with `repo::link_for` and not written back.

use std::collections::HashMap;
use std::path::Path;
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

    /// This node's registered project directories.
    pub fn project_dirs(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT abs_path FROM projects WHERE node_id = ?1 ORDER BY abs_path")?;
        let rows = stmt.query_map([&self.node_id], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<String>>>()?)
    }

    /// Check `groups` against live holds. `agent` (`name` or `name@project`) is the
    /// committer: its own holds are left out when it resolves to an agent of this node.
    /// Work stops at `deadline`; the result then says it is incomplete.
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
        let agent_id = match agent {
            None => None,
            Some(who) => match resolve_agent(&self.conn, &self.node_id, who) {
                Ok(a) => {
                    out.agent = Some(a.address.clone());
                    Some(a.id)
                }
                Err(e) => {
                    out.agent_note = Some(format!("{who}: {e}"));
                    None
                }
            },
        };
        let now = crate::time::now_ms();
        let (links, complete) = self.effective_links(now, deadline)?;
        out.links_complete = complete;
        let holds = self.live_holds(now, agent_id.as_deref())?;

        for group in groups {
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
            if Instant::now() >= deadline {
                out.links_complete = false;
                for p in &group.paths {
                    out.skipped.push(CommitSkip {
                        path: p.clone(),
                        reason: "time budget".into(),
                    });
                }
                continue;
            }
            let mine = match links.get(&project.project_id) {
                Some(l) => l.clone(),
                None => self.link_of(&project)?,
            };
            let (my_key, my_prefix) =
                mine.map_or((String::new(), String::new()), |l| (l.key, l.prefix));
            let fold = folds_case(Path::new(&project.abs_path));
            for raw in &group.paths {
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
        Ok(out)
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
            match cached(&self.conn, &p.project_id)? {
                Some(link) => {
                    links.insert(p.project_id.clone(), link);
                }
                None => {
                    if Instant::now() >= deadline {
                        return Ok((links, false));
                    }
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
