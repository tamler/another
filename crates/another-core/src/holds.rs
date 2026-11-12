//! Path holds: advisory leases on files or globs with a TTL.
//!
//! Holds signal intent; they do not lock files. An exclusive hold conflicts with any
//! overlapping hold of another agent; shared holds only conflict with exclusive ones.
//! Holds lapse when their TTL passes without a heartbeat and are then expired by the sweeper.
//!
//! A hold belongs to a project, but conflicts are checked across every local project in the
//! same git repository (see `repo.rs`), comparing repository-relative paths, so sibling
//! worktrees and clones of one repository see each other's holds.

use std::path::Path;

use globset::{Glob, GlobBuilder, GlobMatcher};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::Serialize;
use serde_json::json;

use crate::error::{Error, Result};
use crate::identity::{address, find_project, resolve_agent, Project};
use crate::mail::{load_message, send_notice_tx, send_tx, touch, Message, NewRequest, SendMessage};
use crate::requests::stale_overdue_tx;
use crate::store::{new_id, Store, Tx};
use crate::time::fmt_ts;
use crate::tokens::TokenScope;

pub const DEFAULT_HOLD_TTL_SECONDS: i64 = 30 * 60;
pub const MIN_HOLD_TTL_SECONDS: i64 = 60;
pub const MAX_HOLD_TTL_SECONDS: i64 = 4 * 60 * 60;

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct Hold {
    pub hold_id: String,
    pub project: String,
    pub agent: String,
    /// Path or glob relative to the project root.
    pub pattern: String,
    pub exclusive: bool,
    pub reason: Option<String>,
    pub ttl_seconds: i64,
    pub created_at: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct HoldConflict {
    /// The requested path or glob.
    pub path: String,
    /// The existing hold it collides with.
    pub held: Hold,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct HoldReport {
    /// Holds now owned by the caller (empty if any conflict was found).
    pub granted: Vec<Hold>,
    pub conflicts: Vec<HoldConflict>,
    /// Overlapping holds on other nodes in a project with the same name. Holds are node-local,
    /// so these never block; they flag a likely cross-machine clash.
    pub warnings: Vec<HoldConflict>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct SweepReport {
    pub expired_holds: usize,
    pub stale_requests: usize,
    pub expired_leases: usize,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct HeartbeatReport {
    pub agent: String,
    pub status: String,
    pub last_seen_at: String,
    pub renewed_holds: Vec<Hold>,
    pub released_holds: Vec<String>,
    /// Resource leases renewed (or released, at session end).
    pub resource_leases: u32,
    pub unread: u32,
    pub urgent_unread: u32,
    /// Requests addressed to or owned by the agent that still need attention.
    pub open_requests: u32,
}

/// Which step of a session a heartbeat represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionPhase {
    Start,
    #[default]
    Beat,
    End,
}

/// Optional context recorded on `session.started` / `session.ended` events.
#[derive(Debug, Clone, Default)]
pub struct SessionInfo {
    /// Client program, e.g. `claude` or `codex`.
    pub client: Option<String>,
    /// The client's own session identifier, if it has one.
    pub session_id: Option<String>,
    /// At session end, keep resource leases (e.g. a server meant to outlive the session).
    pub keep_resources: bool,
}

/// Input for [`Store::acquire_holds`].
#[derive(Debug, Clone)]
pub struct AcquireHolds {
    pub agent: String,
    /// Absolute project path or project slug.
    pub project: String,
    pub paths: Vec<String>,
    pub ttl_seconds: Option<i64>,
    pub exclusive: bool,
    pub reason: Option<String>,
    /// Allow a pattern that covers the whole project (`**`, the project root, `**/*.rs`).
    /// Refused by default: such a hold blocks every other agent in the repository.
    pub repo_wide: bool,
}

/// What [`Store::guard_edit`] found and did.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct EditGuard {
    pub agent: String,
    /// Slug of the project the edited files belong to.
    pub project: String,
    /// Holds taken for these edits: new narrow holds, or an own hold renewed because it was
    /// past half its TTL.
    pub held: Vec<Hold>,
    /// Requested paths already covered by a live exclusive hold of the agent's own.
    pub covered: Vec<String>,
    /// Other agents' overlapping holds. When this is not empty nothing was taken.
    pub conflicts: Vec<HoldConflict>,
    /// First notice for a new set of blockers; repeated attempts get a short denial.
    pub first_notice: bool,
    /// Advisory backoff, in seconds. A wait also sends one inbox ping when the file is free.
    pub retry_after_seconds: i64,
}

#[derive(Debug, Clone, Default)]
pub struct HoldFilter {
    pub project: Option<String>,
    pub agent: Option<String>,
}

const HOLD_SELECT: &str =
    "SELECT h.hold_id, p.slug, a.address, NULL, h.pattern, h.exclusive, h.reason,
       h.ttl_seconds, h.created_at, h.expires_at
     FROM holds h
     JOIN projects p ON p.project_id = h.project_id
     JOIN agents a ON a.agent_id = h.agent_id
     LEFT JOIN projects ap ON ap.project_id = a.project_id";

/// [`HOLD_SELECT`] plus the hold's project id (column 10) and that project's position in its
/// repository (column 11).
const HOLD_SELECT_REPO: &str =
    "SELECT h.hold_id, p.slug, a.address, NULL, h.pattern, h.exclusive, h.reason,
       h.ttl_seconds, h.created_at, h.expires_at, h.project_id, coalesce(pr.repo_prefix, '')
     FROM holds h
     JOIN projects p ON p.project_id = h.project_id
     JOIN agents a ON a.agent_id = h.agent_id
     LEFT JOIN project_repos pr ON pr.project_id = h.project_id";

fn hold_from_row(r: &Row<'_>) -> rusqlite::Result<Hold> {
    let name: String = r.get(2)?;
    let agent_slug: Option<String> = r.get(3)?;
    Ok(Hold {
        hold_id: r.get(0)?,
        project: r.get(1)?,
        agent: address(&name, agent_slug.as_deref()),
        pattern: r.get(4)?,
        exclusive: r.get(5)?,
        reason: r.get(6)?,
        ttl_seconds: r.get(7)?,
        created_at: fmt_ts(r.get(8)?),
        expires_at: fmt_ts(r.get(9)?),
    })
}

fn live_holds(
    conn: &Connection,
    where_sql: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<Vec<Hold>> {
    let mut stmt = conn.prepare(&format!(
        "{HOLD_SELECT} WHERE h.released_at IS NULL AND {where_sql} ORDER BY h.created_at, h.hold_id"
    ))?;
    let rows = stmt.query_map(args, hold_from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn clamp_ttl(ttl: Option<i64>) -> Result<i64> {
    let ttl = ttl.unwrap_or(DEFAULT_HOLD_TTL_SECONDS);
    if ttl <= 0 {
        return Err(Error::Invalid("ttl_seconds must be positive".into()));
    }
    Ok(ttl.clamp(MIN_HOLD_TTL_SECONDS, MAX_HOLD_TTL_SECONDS))
}

/// Normalize a requested path or glob to a project-relative pattern.
pub(crate) fn normalize_pattern(project: &Project, raw: &str) -> Result<String> {
    let path = normalize_path(project, raw)?;
    let pattern = if path.is_empty() {
        "**".to_string()
    } else {
        path
    };
    compile(&pattern)?;
    Ok(pattern)
}

/// Normalize a path without interpreting filename characters as glob syntax.
fn normalize_path(project: &Project, raw: &str) -> Result<String> {
    let raw = raw.trim();
    let root = project.abs_path.trim_end_matches('/');
    let rel = if Path::new(raw).is_absolute() {
        match raw.strip_prefix(root) {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => rest.trim_start_matches('/'),
            _ => {
                return Err(Error::Invalid(format!(
                    "path {raw:?} is outside project {}",
                    project.abs_path
                )))
            }
        }
    } else {
        raw
    };
    let mut parts = Vec::new();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                return Err(Error::Invalid(format!(
                    "path {raw:?} must not contain '..'"
                )))
            }
            p => parts.push(p),
        }
    }
    Ok(parts.join("/"))
}

fn compile(pattern: &str) -> Result<GlobMatcher> {
    let glob: Glob = GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|e| Error::Invalid(format!("invalid glob {pattern:?}: {e}")))?;
    Ok(glob.compile_matcher())
}

/// Decode an entirely literal pattern, including globset's single-character escapes.
/// Real character classes, wildcards and alternations still return None.
fn literal_pattern(pattern: &str) -> Option<String> {
    let mut chars = pattern.chars();
    let mut out = String::new();
    while let Some(c) = chars.next() {
        if c == '[' {
            let escaped = chars.next()?;
            if !matches!(escaped, '*' | '?' | '[' | ']' | '{' | '}') || chars.next()? != ']' {
                return None;
            }
            out.push(escaped);
        } else if matches!(c, '*' | '?' | '{') {
            return None;
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// A pattern that reaches across the whole project: `**` (also what the project root
/// normalizes to), or a glob that starts with a wildcard and contains `**`, like `**/*.rs`.
pub fn is_repo_wide(pattern: &str) -> bool {
    literal_prefix(pattern).is_empty() && pattern.contains("**")
}

/// An own hold `held` already covers the literal path `path`.
fn hold_covers(held: &str, path: &str) -> bool {
    let path = literal_pattern(path).unwrap_or_else(|| path.to_string());
    if held == path {
        return true;
    }
    match literal_pattern(held) {
        Some(held) => covers(&held, &path),
        None => held == "**" || matches_with_literal_prefix(held, &path) || matches(held, &path),
    }
}

/// The part of a pattern before its first glob metacharacter.
fn literal_prefix(pattern: &str) -> &str {
    match pattern.find(['*', '?', '[', '{']) {
        Some(i) => &pattern[..i],
        None => pattern,
    }
}

/// `dir` names `path` itself or a directory containing it.
fn covers(dir: &str, path: &str) -> bool {
    path == dir || path.starts_with(&format!("{dir}/"))
}

/// Conservative overlap test between two project-relative patterns. A plain path also
/// covers everything below it, so holding `src` overlaps `src/main.rs`. False positives
/// are acceptable for advisory holds; false negatives are not.
pub fn patterns_overlap(a: &str, b: &str) -> bool {
    if a == b || a == "**" || b == "**" {
        return true;
    }
    // Older auto holds stored filenames without escapes. Keep their exact spelling
    // protected while also interpreting any deliberate manual glob conservatively.
    if literal_pattern(a).as_deref() == Some(b) || literal_pattern(b).as_deref() == Some(a) {
        return true;
    }
    match (literal_pattern(a), literal_pattern(b)) {
        (Some(a), Some(b)) => covers(&a, &b) || covers(&b, &a),
        (None, Some(b)) => glob_touches_literal(a, &b),
        (Some(a), None) => glob_touches_literal(b, &a),
        (None, None) => {
            let (pa, pb) = (literal_prefix(a), literal_prefix(b));
            pa.starts_with(pb) || pb.starts_with(pa)
        }
    }
}

/// `patterns_overlap`, widened on a volume that ignores letter case (`fold`), where
/// `src/new.rs` and `src/NEW.RS` are one file. Existing files already arrive in their
/// on-disk case; this catches new ones. Folding only ever adds overlaps, and it is skipped
/// when either pattern has a character class: lowercasing `[!a-z]` would change which
/// names the class excludes.
fn overlap_on_volume(a: &str, b: &str, fold: bool) -> bool {
    patterns_overlap(a, b)
        || (fold
            && (literal_pattern(a).is_some() || !a.contains('['))
            && (literal_pattern(b).is_some() || !b.contains('['))
            && patterns_overlap(&a.to_lowercase(), &b.to_lowercase()))
}

/// Whether the volume holding the existing directory `dir` treats names that differ only
/// in letter case as the same entry (APFS and NTFS by default, ext4 not). Found without
/// writing: the last name of `dir` with letters is respelled in the other case, and the
/// result counts only if it is the very same directory. Unknown counts as case-sensitive.
fn folds_case(dir: &Path) -> bool {
    let Ok(real) = std::fs::metadata(dir) else {
        return false;
    };
    let mut cur = dir;
    let mut tail = Vec::new();
    while let (Some(name), Some(parent)) = (cur.file_name(), cur.parent()) {
        let name = name.to_string_lossy();
        let upper = name.to_uppercase();
        let other = if upper != name {
            upper
        } else {
            name.to_lowercase()
        };
        if other != name {
            let mut probe = parent.join(other);
            for t in tail.iter().rev() {
                probe.push(t);
            }
            return std::fs::metadata(&probe).is_ok_and(|m| same_entry(&real, &m));
        }
        tail.push(name.into_owned());
        cur = parent;
    }
    false
}

#[cfg(unix)]
fn same_entry(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(not(unix))]
fn same_entry(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    a.is_dir() && b.is_dir()
}

fn matches(glob: &str, path: &str) -> bool {
    // Patterns are validated on the way in; treat an unparsable one as overlapping.
    compile(glob).map(|m| m.is_match(path)).unwrap_or(true)
}

/// Equal leading components also carry literal directory intent, even when their names
/// contain brackets or braces. At the first differing (wildcard) component, match the
/// entire remaining glob normally. An exhausted side may be an ancestor directory hold.
fn matches_with_literal_prefix(glob: &str, literal: &str) -> bool {
    let pattern: Vec<&str> = glob.split('/').collect();
    let path: Vec<&str> = literal.split('/').collect();
    let shared = pattern
        .iter()
        .zip(&path)
        .take_while(|(a, b)| a == b)
        .count();
    shared == pattern.len()
        || shared == path.len()
        || matches(&pattern[shared..].join("/"), &path[shared..].join("/"))
}

fn glob_touches_literal(glob: &str, literal: &str) -> bool {
    if matches_with_literal_prefix(glob, literal) || matches(glob, literal) {
        return true;
    }
    // The literal may be a directory with matches of the glob below it: compare the
    // glob's leading components at the literal's depth.
    let comps: Vec<&str> = glob.split('/').collect();
    let lit: Vec<&str> = literal.split('/').collect();
    if let Some(pos) = comps.iter().position(|c| c.contains("**")) {
        if pos < lit.len() {
            return pos == 0 || matches(&comps[..pos].join("/"), &lit[..pos].join("/"));
        }
    }
    comps.len() > lit.len() && matches(&comps[..lit.len()].join("/"), literal)
}

/// Link local projects that hold live holds but have no current repository link: never
/// linked (registered before links existed), or linked under older key rules (see
/// `repo::KEY_VERSION`). Their holds then count across the repository straight away.
fn link_unlinked(tx: &Tx<'_>) -> Result<()> {
    let mut stmt = tx.prepare(
        "SELECT DISTINCT p.project_id, p.node_id, p.abs_path, p.slug, p.created_at
         FROM holds h JOIN projects p ON p.project_id = h.project_id
         LEFT JOIN project_repos pr ON pr.project_id = p.project_id
         WHERE h.released_at IS NULL AND h.expires_at > ?1 AND p.node_id = ?2
           AND (pr.project_id IS NULL
                OR (pr.repo_key != '' AND substr(pr.repo_key, 1, length(?3)) != ?3))",
    )?;
    let projects = stmt
        .query_map(params![tx.now, tx.node_id, crate::repo::KEY_VERSION], |r| {
            Ok(Project {
                project_id: r.get(0)?,
                node_id: r.get(1)?,
                abs_path: r.get(2)?,
                slug: r.get(3)?,
                created_at: fmt_ts(r.get(4)?),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for p in &projects {
        crate::repo::refresh_link(tx, p)?;
    }
    Ok(())
}

fn conflicts_for(
    tx: &Tx<'_>,
    project: &Project,
    agent_id: &str,
    patterns: &[(String, String)],
    exclusive: bool,
) -> Result<Vec<HoldConflict>> {
    link_unlinked(tx)?;
    let (repo_key, my_prefix) = crate::repo::link_of(tx, project)?
        .map_or((String::new(), String::new()), |l| {
            (crate::repo::stored_key(&l.key), l.prefix)
        });
    // This project's holds, plus holds in any local project of the same repository.
    let mut stmt = tx.prepare(&format!(
        "{HOLD_SELECT_REPO} WHERE h.released_at IS NULL AND h.expires_at > ?3
           AND h.agent_id != ?2
           AND (h.project_id = ?1 OR (?4 != '' AND h.project_id IN
                (SELECT project_id FROM project_repos WHERE repo_key = ?4)))
         ORDER BY h.created_at"
    ))?;
    let others = stmt
        .query_map(
            params![project.project_id, agent_id, tx.now, repo_key],
            |r| {
                Ok((
                    hold_from_row(r)?,
                    r.get::<_, String>(10)?,
                    r.get::<_, String>(11)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let fold = folds_case(Path::new(&project.abs_path));
    let overlaps = |a: &str, b: &str| overlap_on_volume(a, b, fold);
    let mut conflicts = Vec::new();
    for (requested, pattern) in patterns {
        for (held, held_project, held_prefix) in &others {
            if !(exclusive || held.exclusive) {
                continue;
            }
            let overlap = if *held_project == project.project_id {
                overlaps(pattern, &held.pattern)
            } else {
                // Compare as paths from the repository's top level.
                overlaps(
                    &crate::repo::in_repo(&my_prefix, pattern),
                    &crate::repo::in_repo(held_prefix, &held.pattern),
                )
            };
            if overlap {
                conflicts.push(HoldConflict {
                    path: requested.clone(),
                    held: held.clone(),
                });
            }
        }
    }
    Ok(conflicts)
}

/// Create a hold, or update and renew the agent's existing hold on the same pattern.
/// Returns the hold id.
#[allow(clippy::too_many_arguments)]
fn upsert_hold_tx(
    tx: &Tx<'_>,
    agent_id: &str,
    actor: &str,
    project_id: &str,
    pattern: &str,
    exclusive: bool,
    reason: Option<&str>,
    ttl: i64,
    auto: bool,
) -> Result<String> {
    let existing: Option<String> = tx
        .query_row(
            "SELECT hold_id FROM holds WHERE agent_id = ?1 AND project_id = ?2
               AND pattern = ?3 AND released_at IS NULL",
            params![agent_id, project_id, pattern],
            |r| r.get(0),
        )
        .ok();
    let expires_at = tx.now + ttl * 1000;
    let mut payload = json!({
        "agent_id": agent_id,
        "project_id": project_id,
        "pattern": pattern,
        "exclusive": exclusive,
        "reason": reason,
        "ttl_seconds": ttl,
        "expires_at": expires_at,
    });
    if auto {
        // Taken by the edit hook rather than asked for; counted by `another stats`.
        payload["auto"] = json!(true);
    }
    Ok(match existing {
        Some(id) => {
            let mut payload = payload;
            payload["hold_id"] = json!(id);
            tx.emit(actor, "hold.updated", payload)?;
            tx.execute(
                "UPDATE holds SET exclusive = ?2, reason = ?3, ttl_seconds = ?4,
                   renewed_at = ?5, expires_at = ?6 WHERE hold_id = ?1",
                params![id, exclusive, reason, ttl, tx.now, expires_at],
            )?;
            id
        }
        None => {
            let id = new_id();
            let mut payload = payload;
            payload["hold_id"] = json!(id);
            tx.emit(actor, "hold.acquired", payload)?;
            tx.execute(
                "INSERT INTO holds (hold_id, node_id, project_id, agent_id, pattern,
                   exclusive, reason, ttl_seconds, created_at, renewed_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10)",
                params![
                    id, tx.node_id, project_id, agent_id, pattern, exclusive, reason, ttl, tx.now,
                    expires_at
                ],
            )?;
            id
        }
    })
}

/// Live holds on other nodes, in a project with the same slug, overlapping `patterns`.
fn remote_clashes(
    tx: &Tx<'_>,
    project: &Project,
    patterns: &[(String, String)],
) -> Result<Vec<HoldConflict>> {
    let mut stmt = tx.prepare(&format!(
        "{HOLD_SELECT} WHERE h.released_at IS NULL AND h.expires_at > ?1 AND h.node_id != ?2
           AND p.slug = ?3 ORDER BY h.created_at"
    ))?;
    let theirs = stmt
        .query_map(params![tx.now, tx.node_id, project.slug], hold_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = Vec::new();
    for (requested, pattern) in patterns {
        for held in &theirs {
            if patterns_overlap(pattern, &held.pattern) {
                out.push(HoldConflict {
                    path: requested.clone(),
                    held: held.clone(),
                });
            }
        }
    }
    Ok(out)
}

/// Register durable file waits and advance backoff only after its current window ends.
/// Returns whether any blocker set is new, and the longest remaining backoff.
fn wait_for_conflicts_tx(
    tx: &Tx<'_>,
    agent: &crate::identity::AgentRef,
    project: &Project,
    conflicts: &[HoldConflict],
) -> Result<(bool, i64)> {
    let mut paths: Vec<&str> = conflicts.iter().map(|c| c.path.as_str()).collect();
    paths.sort_unstable();
    paths.dedup();
    let mut first_notice = false;
    let mut retry_after = 0;
    for path in paths {
        let mut blockers: Vec<&str> = conflicts
            .iter()
            .filter(|c| c.path == path)
            .map(|c| c.held.hold_id.as_str())
            .collect();
        blockers.sort_unstable();
        blockers.dedup();
        let signature = serde_json::to_string(&blockers)?;
        let old: Option<(String, i64, i64)> = tx
            .query_row(
                "SELECT blockers, retry_at, backoff_seconds FROM hold_waits
             WHERE agent_id = ?1 AND project_id = ?2 AND path = ?3",
                params![agent.id, project.project_id, path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let is_new = old.as_ref().is_none_or(|(s, _, _)| *s != signature);
        let (retry_at, backoff) = match old {
            Some((_, until, backoff)) if !is_new && until > tx.now => (until, backoff),
            old => {
                let backoff = match old {
                    Some((_, _, previous)) if !is_new => (previous * 2).min(300),
                    _ => 30,
                };
                let until = tx.now + backoff * 1000;
                // The hold row carries the holder's id. Its display address may have become
                // ambiguous since the hold was taken and is never resolved again here.
                let first = conflicts
                    .iter()
                    .find(|c| c.path == path)
                    .expect("path came from conflicts");
                let holder_id: String = tx.query_row(
                    "SELECT agent_id FROM holds WHERE hold_id = ?1",
                    [&first.held.hold_id],
                    |r| r.get(0),
                )?;
                tx.emit(
                    &agent.address,
                    "hold.waiting",
                    json!({
                        "agent_id": agent.id, "project_id": project.project_id,
                        "path": path, "blockers": blockers, "retry_at": until,
                    }),
                )?;
                tx.execute(
                    "INSERT INTO hold_waits (agent_id, project_id, path, blockers, holder_id, retry_at, backoff_seconds)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(agent_id, project_id, path) DO UPDATE SET
                       blockers = excluded.blockers, holder_id = excluded.holder_id,
                       retry_at = excluded.retry_at, backoff_seconds = excluded.backoff_seconds",
                    params![agent.id, project.project_id, path, signature, holder_id, until, backoff],
                )?;
                (until, backoff)
            }
        };
        first_notice |= is_new;
        retry_after = retry_after.max(((retry_at - tx.now + 999) / 1000).min(backoff));
    }
    Ok((first_notice, retry_after))
}

/// Run `f` inside a savepoint. On error only its own work is rolled back and the error is
/// returned; the surrounding transaction stays usable. The event chain is unaffected, since
/// each emit reads its predecessor from the table.
fn attempt<T>(tx: &Tx<'_>, name: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    tx.execute_batch(&format!("SAVEPOINT {name}"))?;
    match f() {
        Ok(v) => {
            tx.execute_batch(&format!("RELEASE SAVEPOINT {name}"))?;
            Ok(v)
        }
        Err(e) => {
            tx.execute_batch(&format!(
                "ROLLBACK TO SAVEPOINT {name}; RELEASE SAVEPOINT {name}"
            ))?;
            Err(e)
        }
    }
}

/// Deliver each free-file ping and remove its wait in the same transaction. Recheck all
/// current blockers, including replacement holds and holds in sibling worktrees. Waiter and
/// holder are addressed by the ids the wait row stores. A ping that cannot be delivered is
/// recorded as `hold.notify_failed` and its wait is dropped anyway, so a release or a sweep
/// never fails, and never fails again later, because of one notice.
fn ping_free_waiters_tx(tx: &Tx<'_>) -> Result<()> {
    let mut stmt = tx.prepare(
        "SELECT w.agent_id, p.slug, w.project_id, w.path, w.holder_id, w.blockers
         FROM hold_waits w JOIN projects p ON p.project_id = w.project_id",
    )?;
    let waits = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (agent_id, project_key, project_id, path, holder_id, blockers) in waits {
        let project = find_project(tx, tx.node_id, &project_key)?;
        if !conflicts_for(
            tx,
            &project,
            &agent_id,
            &[(path.clone(), globset::escape(&path))],
            true,
        )?
        .is_empty()
        {
            continue;
        }
        // Naming the ended holds makes every ping distinct, so the push dedupe never
        // swallows a second ping for the same file within its five-minute window.
        let ended: Vec<String> = serde_json::from_str(&blockers).unwrap_or_default();
        let notice = SendMessage {
            subject: format!("Hold free: {}", path.chars().take(280).collect::<String>()),
            body: format!(
                "Another automatically checked {project_key}:{path}: no conflicting hold remains (hold {} ended). The file is not reserved; check again before editing.",
                ended.join(", ")
            ),
            ..Default::default()
        };
        let sent = attempt(tx, "ping", || {
            tx.emit(
                "system",
                "hold.available",
                json!({
                    "agent_id": agent_id, "project_id": project_id, "path": path,
                    "blockers": ended,
                }),
            )?;
            send_notice_tx(tx, &holder_id, &agent_id, &notice)
        });
        if let Err(e) = sent {
            tx.emit(
                "system",
                "hold.notify_failed",
                json!({
                    "agent_id": agent_id, "project_id": project_id, "path": path,
                    "error": e.to_string(),
                }),
            )?;
        }
        tx.execute(
            "DELETE FROM hold_waits WHERE agent_id = ?1 AND project_id = ?2 AND path = ?3",
            params![agent_id, project_id, path],
        )?;
    }
    Ok(())
}

/// Fulfil only requests explicitly linked to a hold, without guessing from mail text. The
/// request closes first; its notice goes from the holder to the requester by id, and a
/// notice that cannot be delivered is recorded as `request.notify_failed` without failing
/// the release or the sweep.
fn complete_hold_requests_tx(tx: &Tx<'_>) -> Result<()> {
    let mut stmt = tx.prepare(
        "SELECT r.request_id, r.message_id, r.title, r.created_by, h.hold_id, h.agent_id
         FROM request_holds rh JOIN requests r ON r.request_id = rh.request_id
         JOIN holds h ON h.hold_id = rh.hold_id
         WHERE h.node_id = ?1 AND r.status IN ('open', 'claimed', 'stale')
           AND (h.released_at IS NOT NULL OR h.expires_at <= ?2)",
    )?;
    let pending = stmt
        .query_map(params![tx.node_id, tx.now], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (rid, mid, title, creator_id, hold_id, owner_id) in pending {
        let note = format!(
            "Another automatically closed this release request: hold {hold_id} is no longer live."
        );
        tx.emit(
            "system",
            "request.completed",
            json!({
                "request_id": rid, "hold_id": hold_id, "reason": "hold ended", "note": note,
            }),
        )?;
        tx.execute(
            "UPDATE requests SET status = 'done', owner_id = coalesce(owner_id, ?2),
            note = ?3, closed_at = ?4, updated_at = ?4 WHERE request_id = ?1",
            params![rid, owner_id, note, tx.now],
        )?;
        let notice = SendMessage {
            subject: format!("Done: {title}"),
            body: note,
            reply_to: Some(mid),
            ..Default::default()
        };
        if let Err(e) = attempt(tx, "request_notice", || {
            send_notice_tx(tx, &owner_id, &creator_id, &notice)
        }) {
            tx.emit(
                "system",
                "request.notify_failed",
                json!({ "request_id": rid, "hold_id": hold_id, "error": e.to_string() }),
            )?;
        }
    }
    Ok(())
}

pub(crate) fn sweep_tx(tx: &Tx<'_>) -> Result<SweepReport> {
    let mut stmt = tx.prepare(
        "SELECT hold_id, pattern, agent_id FROM holds
         WHERE node_id = ?1 AND released_at IS NULL AND expires_at <= ?2",
    )?;
    let expired = stmt
        .query_map(params![tx.node_id, tx.now], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (hold_id, pattern, agent_id) in &expired {
        tx.emit(
            "system",
            "hold.expired",
            json!({ "hold_id": hold_id, "pattern": pattern, "agent_id": agent_id }),
        )?;
        tx.execute(
            "UPDATE holds SET released_at = expires_at, release_reason = 'expired' WHERE hold_id = ?1",
            [hold_id],
        )?;
    }
    complete_hold_requests_tx(tx)?;
    ping_free_waiters_tx(tx)?;
    let stale_requests = stale_overdue_tx(tx)?;
    let expired_leases = crate::resources::expire_leases_tx(tx)?;
    Ok(SweepReport {
        expired_holds: expired.len(),
        stale_requests,
        expired_leases,
    })
}

pub(crate) fn release_tx(
    tx: &Tx<'_>,
    actor: &str,
    hold_ids: &[String],
    reason: &str,
) -> Result<()> {
    if hold_ids.is_empty() {
        return Ok(());
    }
    tx.emit(
        actor,
        "hold.released",
        json!({ "hold_ids": hold_ids, "reason": reason }),
    )?;
    for id in hold_ids {
        tx.execute(
            "UPDATE holds SET released_at = ?2, release_reason = ?3 WHERE hold_id = ?1",
            params![id, tx.now, reason],
        )?;
    }
    complete_hold_requests_tx(tx)?;
    ping_free_waiters_tx(tx)?;
    Ok(())
}

fn renew_tx(tx: &Tx<'_>, actor: &str, holds: &[(String, i64)]) -> Result<()> {
    if holds.is_empty() {
        return Ok(());
    }
    let ids: Vec<&String> = holds.iter().map(|(id, _)| id).collect();
    tx.emit(actor, "hold.renewed", json!({ "hold_ids": ids }))?;
    for (id, ttl) in holds {
        tx.execute(
            "UPDATE holds SET renewed_at = ?2, expires_at = ?2 + ?3 * 1000 WHERE hold_id = ?1",
            params![id, tx.now, ttl],
        )?;
    }
    Ok(())
}

/// Live holds owned by `agent_id` in projects `scope` may see (all of them locally),
/// optionally limited to `only` ids (error on unknown ids; one outside the scope is unknown).
fn owned_holds(
    tx: &Tx<'_>,
    agent_id: &str,
    only: &[String],
    scope: Option<&TokenScope>,
) -> Result<Vec<(String, i64)>> {
    let mut stmt = tx.prepare(
        "SELECT hold_id, ttl_seconds, project_id FROM holds
         WHERE agent_id = ?1 AND released_at IS NULL ORDER BY created_at",
    )?;
    let rows = stmt
        .query_map([agent_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut owned = Vec::new();
    for (id, ttl, project) in rows {
        let visible = match scope {
            Some(s) => crate::tokens::scope_allows_project(tx, s, &project)?,
            None => true,
        };
        if visible {
            owned.push((id, ttl));
        }
    }
    if only.is_empty() {
        return Ok(owned);
    }
    only.iter()
        .map(|id| {
            owned
                .iter()
                .find(|(h, _)| h == id)
                .cloned()
                .ok_or_else(|| Error::NotFound(format!("no live hold {id:?} owned by this agent")))
        })
        .collect()
}

fn holds_by_id(conn: &Connection, ids: &[String]) -> Result<Vec<Hold>> {
    ids.iter()
        .map(|id| {
            Ok(conn.query_row(
                &format!("{HOLD_SELECT} WHERE h.hold_id = ?1"),
                [id],
                hold_from_row,
            )?)
        })
        .collect()
}

impl Store {
    /// Ask a live local hold's owner to release it. Reuse an outstanding ask from this
    /// agent for this hold. The linked request closes automatically when the hold ends.
    pub fn request_hold_release(&self, agent: &str, hold_id: &str) -> Result<Message> {
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let held = tx.query_row(&format!("{HOLD_SELECT} WHERE h.hold_id = ?1
                AND h.node_id = ?2 AND h.released_at IS NULL AND h.expires_at > ?3"),
                params![hold_id, tx.node_id, tx.now], hold_from_row).optional()?
                .ok_or_else(|| Error::NotFound(format!("unknown live local hold {hold_id:?}")))?;
            if held.agent == me.address {
                return Err(Error::Invalid("release your own hold with hold release".into()));
            }
            let existing: Option<String> = tx.query_row(
                "SELECT r.message_id FROM request_holds rh JOIN requests r ON r.request_id = rh.request_id
                 WHERE rh.hold_id = ?1 AND r.created_by = ?2 AND r.status IN ('open', 'claimed', 'stale')",
                params![hold_id, me.id], |r| r.get(0)).optional()?;
            if let Some(mid) = existing { return load_message(tx, &mid); }
            let pattern = literal_pattern(&held.pattern).unwrap_or(held.pattern);
            let title: String = pattern.chars().take(280).collect();
            let message = send_tx(tx, &SendMessage {
                from: me.address, to: vec![held.agent], subject: format!("Release {title}?"),
                body: format!("Please release hold {hold_id} on {}:{pattern} when your edit is done. Another will close this request when that hold ends.", held.project),
                request: Some(NewRequest::default()), ..Default::default()
            })?;
            let rid = message.request_id.as_ref().expect("request was created");
            tx.emit(&message.from, "request.hold_linked", json!({ "request_id": rid, "hold_id": hold_id }))?;
            tx.execute("INSERT INTO request_holds (request_id, hold_id) VALUES (?1, ?2)", params![rid, hold_id])?;
            Ok(message)
        })
    }

    /// Expire lapsed holds and mark overdue requests stale.
    pub fn sweep(&self) -> Result<SweepReport> {
        self.write(sweep_tx)
    }

    /// Acquire holds on paths/globs. All-or-nothing: if any requested path conflicts with
    /// another agent's live hold, nothing is granted and the conflicts are reported.
    pub fn acquire_holds(&self, input: AcquireHolds) -> Result<HoldReport> {
        if input.paths.is_empty() {
            return Err(Error::Invalid("no paths given".into()));
        }
        let ttl = clamp_ttl(input.ttl_seconds)?;
        self.write(|tx| {
            sweep_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, &input.agent)?;
            let project = find_project(tx, tx.node_id, &input.project)?;
            let mut patterns: Vec<(String, String)> = Vec::new();
            for raw in &input.paths {
                let p = normalize_pattern(&project, raw)?;
                if is_repo_wide(&p) && !input.repo_wide {
                    return Err(Error::Invalid(format!(
                        "{raw:?} would hold the whole project ({p}) and block every other agent in \
                         the repository; hold the files or directories you will change, such as \
                         src/api, or pass --repo-wide (MCP: repo_wide) if you really mean all of it"
                    )));
                }
                if !patterns.iter().any(|(_, q)| *q == p) {
                    patterns.push((raw.clone(), p));
                }
            }
            let conflicts = conflicts_for(tx, &project, &me.id, &patterns, input.exclusive)?;
            let warnings = remote_clashes(tx, &project, &patterns)?;
            if !conflicts.is_empty() {
                touch(tx, &me.id)?;
                return Ok(HoldReport {
                    granted: Vec::new(),
                    conflicts,
                    warnings,
                });
            }
            let mut granted_ids = Vec::new();
            for (_, pattern) in &patterns {
                // Re-acquiring a pattern you already hold updates and renews that hold.
                granted_ids.push(upsert_hold_tx(
                    tx,
                    &me.id,
                    &me.address,
                    &project.project_id,
                    pattern,
                    input.exclusive,
                    input.reason.as_deref(),
                    ttl,
                    false,
                )?);
            }
            touch(tx, &me.id)?;
            Ok(HoldReport {
                granted: holds_by_id(tx, &granted_ids)?,
                conflicts: Vec::new(),
                warnings,
            })
        })
    }

    /// Report which of `paths` would conflict for `agent` (no holds are taken).
    pub fn check_holds(
        &self,
        agent: &str,
        project: &str,
        paths: &[String],
        exclusive: bool,
    ) -> Result<Vec<HoldConflict>> {
        self.write(|tx| {
            sweep_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let project = find_project(tx, tx.node_id, project)?;
            let patterns = paths
                .iter()
                .map(|raw| Ok((raw.clone(), normalize_pattern(&project, raw)?)))
                .collect::<Result<Vec<_>>>()?;
            conflicts_for(tx, &project, &me.id, &patterns, exclusive)
        })
    }

    /// Make sure `agent` may edit `paths` (files in `project`) and hold them while it does.
    ///
    /// If another agent holds an overlapping path (in this project or a sibling checkout of
    /// the same repository), nothing is taken and the conflicts are returned. Otherwise each
    /// path already under a live exclusive hold of the agent's own is left alone (that hold
    /// is renewed once it is past half its TTL), and every other path gets a narrow exclusive
    /// hold of the default TTL. Writes nothing when every path is already covered.
    pub fn guard_edit(
        &self,
        agent: &str,
        project: &str,
        paths: &[String],
        reason: &str,
    ) -> Result<EditGuard> {
        if paths.is_empty() {
            return Err(Error::Invalid("no paths given".into()));
        }
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let project = find_project(tx, tx.node_id, project)?;
            let mut patterns: Vec<(String, String)> = Vec::new();
            for raw in paths {
                let literal = normalize_path(&project, raw)?;
                if literal.is_empty() || raw.ends_with('/') || Path::new(&project.abs_path).join(&literal).is_dir() {
                    return Err(Error::Invalid(format!(
                        "{raw:?} is a directory, not an edited file; request directory holds explicitly with hold acquire or hold_paths"
                    )));
                }
                let p = globset::escape(&literal);
                if !patterns.iter().any(|(_, q)| *q == p) {
                    patterns.push((raw.clone(), p));
                }
            }
            let mut conflicts = conflicts_for(tx, &project, &me.id, &patterns, true)?;
            if !conflicts.is_empty() {
                // Report project-relative paths, like hold patterns, not what the tool sent
                // (often an absolute path).
                for c in &mut conflicts {
                    if let Some((_, p)) = patterns.iter().find(|(raw, _)| *raw == c.path) {
                        c.path = literal_pattern(p).expect("edit targets are escaped literals");
                    }
                }
                // Recording the wait is bookkeeping. A real conflict is denied even when
                // that fails; the failure is an event, never an open guard.
                let (first_notice, retry_after_seconds) = match attempt(tx, "wait", || {
                    wait_for_conflicts_tx(tx, &me, &project, &conflicts)
                }) {
                    Ok(outcome) => outcome,
                    Err(e) => {
                        tx.emit(
                            &me.address,
                            "hold.wait_failed",
                            json!({
                                "agent_id": me.id, "project_id": project.project_id,
                                "error": e.to_string(),
                            }),
                        )?;
                        (true, 30)
                    }
                };
                // Count each distinct collision, rather than every retry of the same edit.
                let repo = crate::repo::link_of(tx, &project)?.map(|l| l.key);
                if first_notice { tx.emit(
                    &me.address,
                    crate::stats::BLOCKED_EVENT,
                    json!({
                        "agent_id": me.id,
                        "agent": me.address,
                        "project_id": project.project_id,
                        "project": project.slug,
                        "repo": repo,
                        "paths": patterns.iter().map(|(_, p)| p).collect::<Vec<_>>(),
                        "conflicts": conflicts.iter().map(|c| json!({
                            "path": c.path,
                            "holder": c.held.agent,
                            "hold_id": c.held.hold_id,
                            "pattern": c.held.pattern,
                            "project": c.held.project,
                        })).collect::<Vec<_>>(),
                    }),
                )?; }
                return Ok(EditGuard {
                    agent: me.address,
                    project: project.slug,
                    held: Vec::new(),
                    covered: Vec::new(),
                    conflicts,
                    first_notice,
                    retry_after_seconds,
                });
            }
            let mut stmt = tx.prepare(
                "SELECT hold_id, pattern, ttl_seconds, expires_at FROM holds
                 WHERE agent_id = ?1 AND project_id = ?2 AND exclusive = 1
                   AND released_at IS NULL AND expires_at > ?3
                 ORDER BY created_at",
            )?;
            let own = stmt
                .query_map(params![me.id, project.project_id, tx.now], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut renew: Vec<(String, i64)> = Vec::new();
            let mut covered = Vec::new();
            let mut needed = Vec::new();
            for (raw, pattern) in &patterns {
                match own
                    .iter()
                    .find(|(_, held, _, _)| hold_covers(held, pattern))
                {
                    Some((id, _, ttl, expires_at)) if expires_at - tx.now < ttl * 500 => {
                        if !renew.iter().any(|(r, _)| r == id) {
                            renew.push((id.clone(), *ttl));
                        }
                    }
                    Some(_) => covered.push(raw.clone()),
                    None => needed.push(pattern.clone()),
                }
            }
            renew_tx(tx, &me.address, &renew)?;
            let mut ids: Vec<String> = renew.into_iter().map(|(id, _)| id).collect();
            for pattern in &needed {
                ids.push(upsert_hold_tx(
                    tx,
                    &me.id,
                    &me.address,
                    &project.project_id,
                    pattern,
                    true,
                    Some(reason),
                    DEFAULT_HOLD_TTL_SECONDS,
                    true,
                )?);
            }
            if !ids.is_empty() {
                touch(tx, &me.id)?;
            }
            for (_, path) in &patterns {
                let path = literal_pattern(path).expect("edit targets are escaped literals");
                let waiting = tx.query_row(
                    "SELECT 1 FROM hold_waits WHERE agent_id = ?1 AND project_id = ?2 AND path = ?3",
                    params![me.id, project.project_id, path], |_| Ok(()),
                ).optional()?.is_some();
                if waiting {
                    tx.emit(&me.address, "hold.wait.finished", json!({ "project_id": project.project_id, "path": path }))?;
                    tx.execute("DELETE FROM hold_waits WHERE agent_id = ?1 AND project_id = ?2 AND path = ?3",
                        params![me.id, project.project_id, path])?;
                }
            }
            Ok(EditGuard {
                agent: me.address,
                project: project.slug,
                held: holds_by_id(tx, &ids)?,
                covered,
                conflicts: Vec::new(),
                first_notice: false,
                retry_after_seconds: 0,
            })
        })
    }

    /// A light heartbeat for hooks that run between tool calls: marks the agent active and
    /// renews only its own live holds that are past half their TTL. No sweep and no session
    /// event; a `hold.renewed` event is written only when something was renewed.
    /// Returns how many holds were renewed.
    pub fn keepalive(&self, agent: &str) -> Result<usize> {
        self.write(|tx| {
            let me = resolve_agent(tx, tx.node_id, agent)?;
            touch(tx, &me.id)?;
            let mut stmt = tx.prepare(
                "SELECT hold_id, ttl_seconds FROM holds
                 WHERE agent_id = ?1 AND released_at IS NULL AND expires_at > ?2
                   AND expires_at - ?2 < ttl_seconds * 500",
            )?;
            let due = stmt
                .query_map(params![me.id, tx.now], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<(String, i64)>>>()?;
            renew_tx(tx, &me.address, &due)?;
            Ok(due.len())
        })
    }

    /// Extend the agent's live holds (all, or just `hold_ids`) by their TTL from now.
    pub fn renew_holds(&self, agent: &str, hold_ids: &[String]) -> Result<Vec<Hold>> {
        self.renew_holds_scoped(agent, hold_ids, None)
    }

    /// [`Store::renew_holds`] for a remote caller: only holds in projects the token may see
    /// (the agent may hold others locally).
    pub fn renew_holds_scoped(
        &self,
        agent: &str,
        hold_ids: &[String],
        scope: Option<&TokenScope>,
    ) -> Result<Vec<Hold>> {
        self.write(|tx| {
            sweep_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let owned = owned_holds(tx, &me.id, hold_ids, scope)?;
            renew_tx(tx, &me.address, &owned)?;
            touch(tx, &me.id)?;
            let ids: Vec<String> = owned.into_iter().map(|(id, _)| id).collect();
            holds_by_id(tx, &ids)
        })
    }

    /// Release the agent's live holds (all, or just `hold_ids`). Returns the released ids.
    pub fn release_holds(&self, agent: &str, hold_ids: &[String]) -> Result<Vec<String>> {
        self.release_holds_scoped(agent, hold_ids, None)
    }

    /// [`Store::release_holds`] for a remote caller: only holds in projects the token may see
    /// (the agent may hold others locally).
    pub fn release_holds_scoped(
        &self,
        agent: &str,
        hold_ids: &[String],
        scope: Option<&TokenScope>,
    ) -> Result<Vec<String>> {
        self.write(|tx| {
            sweep_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let ids: Vec<String> = owned_holds(tx, &me.id, hold_ids, scope)?
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            release_tx(tx, &me.address, &ids, "released")?;
            touch(tx, &me.id)?;
            Ok(ids)
        })
    }

    /// Live holds, optionally filtered by project and/or agent. A project filter also lists
    /// holds in sibling checkouts of the same repository, since those conflict too.
    pub fn holds(&self, filter: HoldFilter) -> Result<Vec<Hold>> {
        self.write(|tx| {
            sweep_tx(tx)?;
            let (project_id, repo_key) = match &filter.project {
                Some(p) => {
                    let project = find_project(tx, tx.node_id, p)?;
                    link_unlinked(tx)?;
                    let key = crate::repo::link_of(tx, &project)?
                        .map(|l| crate::repo::stored_key(&l.key))
                        .unwrap_or_default();
                    (Some(project.project_id), key)
                }
                None => (None, String::new()),
            };
            let agent_id = match &filter.agent {
                Some(a) => Some(resolve_agent(tx, tx.node_id, a)?.id),
                None => None,
            };
            live_holds(
                tx,
                "h.node_id = ?1 AND (?3 IS NULL OR h.agent_id = ?3)
                 AND (?2 IS NULL OR h.project_id = ?2 OR (?4 != '' AND h.project_id IN
                      (SELECT project_id FROM project_repos WHERE repo_key = ?4)))",
                &[&tx.node_id, &project_id, &agent_id, &repo_key],
            )
        })
    }

    /// Session heartbeat: touch `last_seen_at` and renew the agent's holds. With `end`,
    /// release every hold instead and mark the agent offline.
    pub fn heartbeat(&self, agent: &str, end: bool) -> Result<HeartbeatReport> {
        let phase = if end {
            SessionPhase::End
        } else {
            SessionPhase::Beat
        };
        self.session(agent, phase, SessionInfo::default())
    }

    /// Record a session lifecycle step. `Start` and `End` append `session.started` /
    /// `session.ended` events; a plain `Beat` only updates presence and renews holds.
    pub fn session(
        &self,
        agent: &str,
        phase: SessionPhase,
        info: SessionInfo,
    ) -> Result<HeartbeatReport> {
        self.session_scoped(agent, phase, info, None)
    }

    /// [`Store::session`] for a remote caller: only holds and resource leases in projects the
    /// token may see are renewed or released (the agent may hold others locally).
    pub fn session_scoped(
        &self,
        agent: &str,
        phase: SessionPhase,
        info: SessionInfo,
        scope: Option<&TokenScope>,
    ) -> Result<HeartbeatReport> {
        self.write(|tx| {
            sweep_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let owned = owned_holds(tx, &me.id, &[], scope)?;
            let session_payload = || {
                json!({
                    "agent_id": me.id,
                    "client": info.client,
                    "session_id": info.session_id,
                })
            };
            let release_leases = phase == SessionPhase::End && !info.keep_resources;
            let resource_leases =
                crate::resources::session_leases_tx(tx, &me, release_leases, scope)?;
            let (renewed, released, status) = if phase == SessionPhase::End {
                let ids: Vec<String> = owned.into_iter().map(|(id, _)| id).collect();
                release_tx(tx, &me.address, &ids, "session_end")?;
                tx.emit(&me.address, "session.ended", session_payload())?;
                tx.execute(
                    "UPDATE agents SET status = 'offline', last_seen_at = ?2 WHERE agent_id = ?1",
                    params![me.id, tx.now],
                )?;
                (Vec::new(), ids, "offline")
            } else {
                if phase == SessionPhase::Start {
                    tx.emit(&me.address, "session.started", session_payload())?;
                }
                renew_tx(tx, &me.address, &owned)?;
                touch(tx, &me.id)?;
                let ids: Vec<String> = owned.into_iter().map(|(id, _)| id).collect();
                (holds_by_id(tx, &ids)?, Vec::new(), "active")
            };
            let (unread, urgent_unread): (u32, u32) = tx.query_row(
                "SELECT count(*), count(*) FILTER (WHERE m.importance IN ('high', 'urgent'))
                 FROM deliveries d JOIN messages m ON m.message_id = d.message_id
                 WHERE d.agent_id = ?1 AND d.read_at IS NULL",
                [&me.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let open_requests = crate::requests::list_requests_for_inbox(tx, &me.id)?.len() as u32;
            Ok(HeartbeatReport {
                agent: me.address,
                status: status.to_string(),
                last_seen_at: fmt_ts(tx.now),
                renewed_holds: renewed,
                released_holds: released,
                resource_leases,
                unread,
                urgent_unread,
                open_requests,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_rules() {
        assert!(patterns_overlap("src/main.rs", "src/main.rs"));
        assert!(patterns_overlap("src", "src/main.rs"));
        assert!(!patterns_overlap("src/main.rs", "src/lib.rs"));
        assert!(!patterns_overlap("src", "srcx/a.rs"));
        assert!(patterns_overlap("src/*.rs", "src/main.rs"));
        assert!(!patterns_overlap("src/*.rs", "src/a/b.rs"));
        assert!(patterns_overlap("src/**", "src/a/b.rs"));
        assert!(patterns_overlap("src/**/*.rs", "src/a"));
        assert!(patterns_overlap("src/**/*.rs", "src"));
        assert!(patterns_overlap("src/**", "src/*.toml"));
        assert!(!patterns_overlap("docs/**", "src/**"));
        assert!(patterns_overlap("**", "anything"));
    }

    #[test]
    fn case_folding_only_adds_overlaps_and_leaves_classes_exact() {
        for fold in [false, true] {
            assert!(overlap_on_volume("src/[!a-z]*.txt", "src/Upper.txt", fold));
            assert!(overlap_on_volume("src/[!A-Z]*.txt", "src/a.txt", fold));
            assert!(!overlap_on_volume("src/[A-Z]*.txt", "src/a.txt", fold));
        }
        assert!(overlap_on_volume("src/*.TXT", "src/a.txt", true));
        assert!(!overlap_on_volume("src/*.TXT", "src/a.txt", false));
        assert!(overlap_on_volume("src/new.txt", "SRC/NEW.TXT", true));
        assert!(!overlap_on_volume("src/new.txt", "SRC/NEW.TXT", false));
    }

    #[test]
    fn repo_wide_patterns() {
        for p in ["**", "**/*.rs", "*/**", "{src,docs}/**"] {
            assert!(is_repo_wide(p), "{p}");
        }
        for p in ["src/**", "src", "*.md", "src/**/*.rs", "README.md"] {
            assert!(!is_repo_wide(p), "{p}");
        }
    }

    #[test]
    fn own_hold_coverage() {
        assert!(hold_covers("src", "src/a.rs"));
        assert!(hold_covers("src/a.rs", "src/a.rs"));
        assert!(hold_covers("src/**", "src/x/y.rs"));
        assert!(hold_covers("src/*.rs", "src/a.rs"));
        assert!(!hold_covers("src/*.rs", "src/x/a.rs"));
        assert!(!hold_covers("srcx", "src/a.rs"));
        assert!(hold_covers("**", "anything/at/all"));
    }

    #[test]
    fn normalizes_patterns() {
        let project = Project {
            project_id: "p".into(),
            node_id: "n".into(),
            abs_path: "/work/repo".into(),
            slug: "repo".into(),
            created_at: String::new(),
        };
        assert_eq!(normalize_pattern(&project, "./src/").unwrap(), "src");
        assert_eq!(
            normalize_pattern(&project, "/work/repo/src/a.rs").unwrap(),
            "src/a.rs"
        );
        assert_eq!(normalize_pattern(&project, "/work/repo").unwrap(), "**");
        assert!(normalize_pattern(&project, "/work/repository/a").is_err());
        assert!(normalize_pattern(&project, "../x").is_err());
        assert!(normalize_pattern(&project, "src/[").is_err());
    }
}
