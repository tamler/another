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
use rusqlite::{params, Connection, Row};
use serde::Serialize;
use serde_json::json;

use crate::error::{Error, Result};
use crate::identity::{address, find_project, resolve_agent, Project};
use crate::mail::touch;
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
        None => held == "**" || glob_touches_literal(held, &path, false),
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
        (None, Some(b)) => glob_touches_literal(a, &b, true),
        (Some(a), None) => glob_touches_literal(b, &a, true),
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

/// One glob component against one literal path component. Three readings count: the same
/// text, a glob match, and a glob match in which `[`, `]`, `{` and `}` are plain characters
/// while `*` and `?` stay wildcards. The last reading is how `[id]*` names the route
/// directories that start with `[id]`, and it keeps a bracketed directory name literal even
/// after a wildcard component, where no leading-prefix rule can reach it. A globset escape
/// such as `[*]` already spells one plain character and is kept as it is.
fn component_touches(glob: &str, path: &str) -> bool {
    // A piece of a split pattern may not compile on its own; then it matches nothing here
    // and only the whole-glob reading in `glob_touches_literal` can count.
    if glob == path || compile(glob).is_ok_and(|m| m.is_match(path)) {
        return true;
    }
    let mut spelled = String::with_capacity(glob.len() + 8);
    let mut rest = glob;
    while let Some(c) = rest.chars().next() {
        let escape = rest
            .get(..3)
            .filter(|e| literal_pattern(e).is_some_and(|l| l.len() == 1));
        if let Some(e) = escape {
            spelled.push_str(e);
            rest = &rest[3..];
            continue;
        }
        if matches!(c, '[' | ']' | '{' | '}') {
            spelled.push('[');
            spelled.push(c);
            spelled.push(']');
        } else {
            spelled.push(c);
        }
        rest = &rest[c.len_utf8()..];
    }
    spelled != glob && compile(&spelled).is_ok_and(|m| m.is_match(path))
}

/// Where the character class opened by the `[` at byte `open` closes: the byte of its `]`,
/// read as globset reads it (`[!` and `[^` negate; a `]` first in the class is a member,
/// so `[]]` is the class of `]`). None for an unclosed class, which is then a plain `[`.
fn class_end(pattern: &str, open: usize) -> Option<usize> {
    let bytes = pattern.as_bytes();
    let mut i = open + 1;
    if matches!(bytes.get(i), Some(b'!' | b'^')) {
        i += 1;
    }
    if bytes.get(i) == Some(&b']') {
        i += 1;
    }
    pattern.get(i..)?.find(']').map(|j| i + j)
}

/// The characters of `pattern` outside character classes, with their byte offsets. A class
/// and everything in it is skipped, so `[*]`, `[/]` and `[{]` are never read as a
/// wildcard, a separator or an alternation.
fn outside_classes(pattern: &str) -> Vec<(usize, char)> {
    let mut out = Vec::new();
    let mut skip_to = 0;
    for (i, c) in pattern.char_indices() {
        if i < skip_to {
            continue;
        }
        if c == '[' {
            if let Some(end) = class_end(pattern, i) {
                skip_to = end + 1;
                continue;
            }
        }
        out.push((i, c));
    }
    out
}

/// A glob component that names a directory or a file rather than a set of them: no `*` and
/// no `?` outside a character class. `[id]`, `{shop,blog}` and the escaped literal names
/// `[*]` and `[?]` qualify, `*` and `[id]*` do not.
fn names_entries(component: &str) -> bool {
    !outside_classes(component)
        .iter()
        .any(|&(_, c)| matches!(c, '*' | '?'))
}

/// A pattern's components: split on `/` outside brackets and braces, so an alternation
/// such as `{a/b,c}` stays one piece (`expand_alternations` spells those out first).
fn pattern_components(pattern: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut start, mut depth) = (0, 0usize);
    for (i, c) in outside_classes(pattern) {
        match c {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            '/' if depth == 0 => {
                out.push(&pattern[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&pattern[start..]);
    out
}

/// The first alternation of `pattern` outside character classes: the text before it, its
/// alternatives, and the text after it. The braces read as plain characters count as one
/// more alternative, spelled `[{]...[}]`, so that `{group}` still names a directory called
/// `{group}`; a name cannot contain a `/`, so that reading is skipped when the alternation
/// does. None without an alternation, and for an unclosed `{`, which globset refuses.
fn first_alternation(pattern: &str) -> Option<(&str, Vec<String>, &str)> {
    let outside = outside_classes(pattern);
    let open = outside.iter().position(|&(_, c)| c == '{')?;
    let open_at = outside[open].0;
    let close_at = outside[open..].iter().find(|&&(_, c)| c == '}')?.0;
    let inside = &pattern[open_at + 1..close_at];
    let mut alternatives = Vec::new();
    let mut start = open_at + 1;
    for &(i, c) in &outside[open + 1..] {
        if i >= close_at {
            break;
        }
        if c == ',' {
            alternatives.push(pattern[start..i].to_string());
            start = i + 1;
        }
    }
    alternatives.push(pattern[start..close_at].to_string());
    if !inside.contains('/') {
        alternatives.push(format!("[{{]{inside}[}}]"));
    }
    Some((&pattern[..open_at], alternatives, &pattern[close_at + 1..]))
}

/// `pattern` with every alternation spelled out, so that an alternative containing a `/`
/// becomes ordinary components: `src/{a/b,c}` is `src/a/b` and `src/c`. A pattern with no
/// alternation comes back as it is. So does one that would spell out to more than
/// `ALTERNATION_CAP` patterns; the whole-glob reading still covers it.
fn expand_alternations(pattern: &str) -> Vec<String> {
    const ALTERNATION_CAP: usize = 256;
    let mut done = Vec::new();
    let mut todo = vec![pattern.to_string()];
    while let Some(p) = todo.pop() {
        match first_alternation(&p) {
            None => done.push(p),
            Some((head, alternatives, tail)) => {
                if done.len() + todo.len() + alternatives.len() > ALTERNATION_CAP {
                    return vec![pattern.to_string()];
                }
                todo.extend(
                    alternatives
                        .into_iter()
                        .map(|alt| format!("{head}{alt}{tail}")),
                );
            }
        }
    }
    done
}

/// Glob components against the components of a literal path. `**` spans zero or more
/// components. A glob whose last component names an entry (`names_entries`) and that ends
/// above the rest of the path is a directory hold, and covers everything below it; `src/*`
/// names the entries directly in `src` and nothing deeper. A path that ends first is an
/// ancestor directory of what the glob names when the caller allows the path to be a
/// directory (`path_may_be_dir`); an edited file is never that.
fn components_touch(glob: &[&str], path: &[&str], path_may_be_dir: bool) -> bool {
    match (glob.split_first(), path.split_first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some(_), None) => path_may_be_dir,
        (Some((&"**", rest)), Some((_, deeper))) => {
            rest.is_empty()
                || components_touch(rest, path, path_may_be_dir)
                || components_touch(glob, deeper, path_may_be_dir)
        }
        (Some((g, rest)), Some((p, deeper))) => {
            component_touches(g, p)
                && if rest.is_empty() {
                    deeper.is_empty() || names_entries(g)
                } else {
                    components_touch(rest, deeper, path_may_be_dir)
                }
        }
    }
}

/// A glob against a literal path: each spelled-out alternation component by component, and
/// the whole as globset reads it.
fn glob_touches_literal(glob: &str, literal: &str, path_may_be_dir: bool) -> bool {
    let lit: Vec<&str> = literal.split('/').collect();
    expand_alternations(glob)
        .iter()
        .any(|g| components_touch(&pattern_components(g), &lit, path_may_be_dir))
        || matches(glob, literal)
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
                // Evidence for `another stats`: who was stopped, by whose hold, where.
                let repo = crate::repo::link_of(tx, &project)?.map(|l| l.key);
                tx.emit(
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
                )?;
                return Ok(EditGuard {
                    agent: me.address,
                    project: project.slug,
                    held: Vec::new(),
                    covered: Vec::new(),
                    conflicts,
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
            Ok(EditGuard {
                agent: me.address,
                project: project.slug,
                held: holds_by_id(tx, &ids)?,
                covered,
                conflicts: Vec::new(),
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

    /// PR #108 r4: a wildcard component before a bracketed or braced directory must not
    /// turn that directory back into a character class or an alternation, in either order.
    #[test]
    fn wildcards_before_bracketed_components_keep_literal_intent() {
        for (glob, path) in [
            (
                "packages/*/app/[id]/page.tsx",
                "packages/web/app/[id]/page.tsx",
            ),
            ("app/*/[id]/*.tsx", "app/shop/[id]/page.tsx"),
            ("app/{shop,blog}/[id]/page.tsx", "app/shop/[id]/page.tsx"),
            ("app/[id]/**/[slug]/*.tsx", "app/[id]/x/[slug]/page.tsx"),
            ("app/[id]/*/[slug]/page.tsx", "app/[id]/x/[slug]/page.tsx"),
            ("app/?/[id]/page.tsx", "app/a/[id]/page.tsx"),
            ("app/[id]*/page.tsx", "app/[id]x/page.tsx"),
            // A glob whose last component names a directory covers the files below it.
            ("packages/*/app/[id]", "packages/web/app/[id]/page.tsx"),
            ("packages/*/app/[id]/**", "packages/web/app/[id]/page.tsx"),
            ("app/{shop,blog}", "app/shop/[id]/page.tsx"),
            // `*` and `src/*` name the entries at their own depth.
            ("*", "top.rs"),
            ("src/*", "src/a.rs"),
            // An alternation may contain a `/`; the whole-glob reading catches it.
            ("src/{a/b,c}.ts", "src/a/b.ts"),
            ("src/{a/b,c}.ts", "src/c.ts"),
            // r6: an alternation with a `/` is spelled out before the components are
            // compared, so it still names a directory or keeps a later route literal.
            ("src/{a/b,c}", "src/a/b/page.tsx"),
            ("src/{a/b,c}", "src/c/page.tsx"),
            ("src/{a/b,c}/[id]/page.tsx", "src/a/b/[id]/page.tsx"),
            ("src/{a/b,c}/[id]/page.tsx", "src/c/[id]/page.tsx"),
            ("src/{a/b,c}/**/[slug]/*.tsx", "src/a/b/x/[slug]/page.tsx"),
            ("{a/b,c}/[id]", "a/b/[id]/page.tsx"),
            // Braces read as plain characters still name a directory called `{group}`.
            (
                "app/{group}/part[ver]/**/file-?.{rs,ts}",
                "app/{group}/part[ver]/deep/file-1.rs",
            ),
            ("app/{group}", "app/{group}/[id]/page.tsx"),
            // r6: `[*]` and `[?]` are the literal names `*` and `?`, so they name a
            // directory the way `[id]` does.
            ("packages/*/app/[*]", "packages/web/app/*/page.tsx"),
            ("packages/*/app/[?]", "packages/web/app/?/page.tsx"),
            ("app/[*]/[id]", "app/*/[id]/page.tsx"),
            // A literal ancestor directory still meets a deeper glob.
            ("packages/*/app/[id]/page.tsx", "packages/web/app"),
            ("packages/*/app/[id]/page.tsx", "packages/web/app/[id]"),
            ("src/{a/b,c}.ts", "src/a"),
            // Below a `**`, a deeper literal may be a directory with matches inside it,
            // as it always was for `src/**/*.rs` against `src/a/b.css`.
            ("app/[id]/**/[slug]/*.tsx", "app/[id]/x/[slug]/page.css"),
            ("src/**/*.rs", "src/a/b.css"),
        ] {
            let literal = globset::escape(path);
            assert!(patterns_overlap(glob, &literal), "{glob} vs {path}");
            assert!(patterns_overlap(&literal, glob), "{path} vs {glob}");
        }
        // Real glob readings still count: a class, an alternation, a wildcard tail.
        assert!(patterns_overlap(
            "app/[ab]/page.tsx",
            &globset::escape("app/a/page.tsx")
        ));
        assert!(patterns_overlap(
            "src/{a,b}.ts",
            &globset::escape("src/a.ts")
        ));
        assert!(patterns_overlap(
            "app/[id]/*.tsx",
            &globset::escape("app/[id]/page.tsx")
        ));
        // Must stay free.
        for (glob, path) in [
            (
                "packages/*/app/[id]/page.tsx",
                "packages/web/app/[idx]/page.tsx",
            ),
            (
                "packages/*/app/[id]/page.tsx",
                "packages/web/lib/[id]/page.tsx",
            ),
            ("app/{shop,blog}/[id]/page.tsx", "app/docs/[id]/page.tsx"),
            ("app/?/[id]/page.tsx", "app/ab/[id]/page.tsx"),
            ("app/[id]*/page.tsx", "app/[idx]/page.tsx"),
            ("app/[id]/**/[slug]/*.tsx", "app/other/[slug]/page.tsx"),
            ("app/[id]/*/[slug]/page.tsx", "app/[id]/x/[slug]/page.css"),
            ("app/*/[id]/*.tsx", "app/shop/[id]/deep/page.tsx"),
            ("app/[id]", "app/[idx]/page.tsx"),
            ("app/[id]", "other/[id]/page.tsx"),
            ("src/*.rs", "src/deep/a.rs"),
            ("src/a?.rs", "src/abc.rs"),
            // r5: a wildcard last component is not a directory hold, so `*`, `*/*` and
            // `src/*` do not act like `**`.
            ("*", "src/a.rs"),
            ("*/*", "a/b/c.rs"),
            ("src/*", "src/deep/a.rs"),
            ("app/[id]*", "app/[id]x/page.tsx"),
            ("app/?", "app/a/page.tsx"),
            // The alternatives name exactly what they spell.
            ("src/{a/b,c}.ts", "src/x.ts"),
            ("src/{a/b,c}.ts", "src/a/x.ts"),
            ("src/{a/b,c}", "src/a/x.ts"),
            ("src/{a/b,c}", "src/ab/page.tsx"),
            ("src/{a/b,c}/[id]/page.tsx", "src/a/b/[idx]/page.tsx"),
            ("src/{a/b,c}/[id]/page.tsx", "src/a/[id]/page.tsx"),
            ("src/{a/b,c}/[id]/page.tsx", "src/c/[id]/page.css"),
            // An escaped wildcard is one literal character, not a wildcard.
            ("packages/*/app/[*]", "packages/web/app/x/page.tsx"),
            ("packages/*/app/[*]", "packages/web/app/*x/page.tsx"),
            ("packages/*/app/[*]", "packages/web/app/[idx]/page.tsx"),
            ("packages/*/app/[?]", "packages/web/app/x/page.tsx"),
            ("packages/*/app/[?]", "packages/web/app/[i]/page.tsx"),
        ] {
            let literal = globset::escape(path);
            assert!(!patterns_overlap(glob, &literal), "{glob} vs {path}");
            assert!(!patterns_overlap(&literal, glob), "{path} vs {glob}");
        }
        assert_eq!(
            pattern_components("src/{a/b,c}/[x/y]/*.ts"),
            vec!["src", "{a/b,c}", "[x/y]", "*.ts"]
        );
        assert_eq!(
            pattern_components("src/[/]x/[!/]y/[]/]z/*.ts"),
            vec!["src", "[/]x", "[!/]y", "[]/]z", "*.ts"]
        );
        let mut spelled = expand_alternations("src/{a/b,c}/[x,y]/{d,}.ts");
        spelled.sort();
        assert_eq!(
            spelled,
            [
                "src/a/b/[x,y]/.ts",
                "src/a/b/[x,y]/[{]d,[}].ts",
                "src/a/b/[x,y]/d.ts",
                "src/c/[x,y]/.ts",
                "src/c/[x,y]/[{]d,[}].ts",
                "src/c/[x,y]/d.ts",
            ]
        );
        assert_eq!(
            expand_alternations("src/[{]a,b[}]/*.ts"),
            ["src/[{]a,b[}]/*.ts"]
        );
        assert_eq!(expand_alternations("src/{a,b"), ["src/{a,b"]);
        for c in [
            "[id]",
            "{shop,blog}",
            "[*]",
            "[?]",
            "[!*?]",
            "[]*]",
            "a[",
            "x",
        ] {
            assert!(names_entries(c), "{c}");
        }
        for c in ["*", "?", "[id]*", "*.rs", "[*]*", "a[b]?"] {
            assert!(!names_entries(c), "{c}");
        }
        // The escaped spelling keeps working and does not read as a class.
        assert!(patterns_overlap(
            "packages/*/app/[[]id[]]/page.tsx",
            &globset::escape("packages/web/app/[id]/page.tsx")
        ));
        assert!(!patterns_overlap(
            "packages/*/app/[[]id[]]/page.tsx",
            &globset::escape("packages/web/app/i/page.tsx")
        ));
    }

    /// Own-hold coverage reads the glob the same way, but the edited path is a file: a glob
    /// that would only match a directory above it does not cover it.
    #[test]
    fn own_hold_coverage_through_wildcards() {
        assert!(hold_covers(
            "packages/*/app/[id]",
            "packages/web/app/[id]/page.tsx"
        ));
        assert!(!hold_covers("app/[id]*", "app/[id]x/page.tsx"));
        assert!(hold_covers("app/[id]*/**", "app/[id]x/page.tsx"));
        assert!(hold_covers("src/*", "src/a.rs"));
        assert!(!hold_covers("src/*", "src/deep/a.rs"));
        assert!(hold_covers(
            "app/{shop,blog}/[id]/page.tsx",
            "app/shop/[id]/page.tsx"
        ));
        assert!(!hold_covers("src/**/*.rs", "src/a"));
        assert!(!hold_covers(
            "packages/*/app/[id]/page.tsx",
            "packages/web/app"
        ));
        // r6: an alternation with a `/`, and the escaped literal names `*` and `?`.
        assert!(hold_covers("src/{a/b,c}", "src/a/b/page.tsx"));
        assert!(hold_covers(
            "src/{a/b,c}/[id]/page.tsx",
            "src/a/b/[id]/page.tsx"
        ));
        assert!(!hold_covers("src/{a/b,c}", "src/a/x.ts"));
        assert!(hold_covers(
            "packages/*/app/[*]",
            "packages/web/app/*/page.tsx"
        ));
        assert!(hold_covers(
            "packages/*/app/[*]",
            &globset::escape("packages/web/app/*/page.tsx")
        ));
        assert!(hold_covers(
            "packages/*/app/[?]",
            "packages/web/app/?/page.tsx"
        ));
        assert!(!hold_covers(
            "packages/*/app/[*]",
            "packages/web/app/x/page.tsx"
        ));
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
