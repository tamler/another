//! Path holds: advisory leases on files or globs with a TTL.
//!
//! Holds signal intent; they do not lock files. An exclusive hold conflicts with any
//! overlapping hold of another agent; shared holds only conflict with exclusive ones.
//! Holds stop excluding at their deadline. A sweep later records that expiry.
//!
//! A hold belongs to a project and blocks only inside that project's checkout: the working
//! directory it was taken in (see `repo.rs`, D92). Projects registered at different
//! directories of one checkout share it, compared by paths from its top; worktrees, clones
//! and nested checkouts of one repository are separate and never see each other's holds.
//! What follows from that for a pattern that points into another checkout, and release by
//! path, is in `checkout.rs`.

use std::path::Path;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::Serialize;
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::identity::{address, find_project, Project};
use crate::mail::{
    load_message, send_by_id_tx, send_notice_tx, touch, Message, NewRequest, SendMessage,
};
use crate::requests::stale_overdue_tx;
use crate::store::{new_id, Store, Tx};
use crate::time::fmt_ts;
use crate::tokens::TokenScope;

pub const DEFAULT_HOLD_TTL_SECONDS: i64 = crate::DEFAULT_TTL_SECONDS;
pub const MIN_HOLD_TTL_SECONDS: i64 = crate::MIN_TTL_SECONDS;
pub const MAX_HOLD_TTL_SECONDS: i64 = crate::MAX_TTL_SECONDS;
/// Most hold ids one `hold.renewed` or `hold.released` event lists (about 310 KiB). A
/// heartbeat or a session end over more holds writes several such events in its one
/// transaction, so neither can meet the size limit for one event (D99). Every reader takes
/// the ids event by event, so several events read as one.
const HOLD_IDS_PER_EVENT: usize = 10_000;
/// Longest hold reason a local caller may give, in bytes (D99).
pub const MAX_HOLD_REASON_BYTES: usize = 1024;
/// Longest hold pattern, in bytes: as long as any real path (D99). 4096 is the longest
/// path Linux takes in one call (macOS stops at 1024). A glob of this size compiles in a
/// few milliseconds. D99 set it for what a local caller gives to `hold acquire`; it is
/// also the most this build compiles as a glob, wherever the pattern came from, and the
/// limit for a pattern a peer sends. The edit hook's holds are file names compared as
/// text, never compiled, and are not held to it.
pub const MAX_HOLD_PATTERN_BYTES: usize = 4096;

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
    /// Set when the existing hold's pattern could not be read (longer than the limit, or
    /// not a glob that compiles), so it counts as overlapping every path in its project.
    /// The text names the holder's node and says how to clear the hold; `held.pattern` is
    /// then a short prefix and a length, never the stored pattern.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unreadable: Option<String>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct HoldReport {
    /// Holds now owned by the caller (empty if any conflict was found).
    pub granted: Vec<Hold>,
    pub conflicts: Vec<HoldConflict>,
    /// Overlapping holds on other nodes in a project with the same name. Holds are node-local,
    /// so these never block; they flag a likely cross-machine clash. At most ten are listed.
    pub warnings: Vec<HoldConflict>,
    /// Overlapping holds on other nodes that were found but are not listed in `warnings`.
    #[serde(skip_serializing_if = "is_zero")]
    pub warnings_more: u32,
    /// Holds on other nodes that were not compared, by node: comparing with other nodes'
    /// holds is best effort within a fixed budget per command, and a peer chooses how many
    /// holds it sends and how costly their patterns are. They may overlap.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub not_compared: Vec<NotCompared>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Holds of one other node that a command did not compare.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct NotCompared {
    /// The node's name.
    pub node: String,
    pub holds: u32,
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
    /// Accepted for source compatibility only. Session end always releases leases.
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
    /// Refused by default: such a hold blocks every other agent in the checkout.
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
    /// A durable wait is on record for these conflicts, so one inbox ping follows when the
    /// file is free. False when recording the wait failed: no ping will come, and the
    /// agent has to check again itself after `retry_after_seconds`. Always false when
    /// there are no conflicts.
    pub wait_recorded: bool,
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
/// checkout (column 11).
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
    crate::checkout_ttl(ttl)
}

/// Normalize a requested path or glob to a project-relative pattern.
pub(crate) fn normalize_pattern(project: &Project, raw: &str) -> Result<String> {
    // Longer than any pattern in this project can be: refused before any message quotes it.
    if raw.trim().len() > MAX_HOLD_PATTERN_BYTES + project.abs_path.len() + 1 {
        crate::queue::within_bytes("hold pattern", raw.trim(), MAX_HOLD_PATTERN_BYTES)?;
    }
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
    #[cfg(windows)]
    let (raw, root_path) = (raw.replace('\\', "/"), project.abs_path.replace('\\', "/"));
    #[cfg(windows)]
    let (raw, root_path) = (raw.as_str(), root_path.as_str());
    #[cfg(not(windows))]
    let root_path = project.abs_path.as_str();
    let root = root_path.trim_end_matches('/');
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

/// A pattern for a message: its first 40 characters as one shown line, quoted, and its
/// length. A pattern can come from a peer or from a row an older build stored, and can be
/// megabytes long, so it is never printed whole.
pub(crate) fn pattern_brief(pattern: &str) -> String {
    let head: String = pattern.chars().take(PATTERN_BRIEF_CHARS + 1).collect();
    format!(
        "{:?} ({} bytes)",
        crate::mail::display_line(&head, PATTERN_BRIEF_CHARS),
        pattern.len()
    )
}

/// How much of a pattern [`pattern_brief`] shows.
const PATTERN_BRIEF_CHARS: usize = 40;

/// A requested glob, prepared to judge holds on plain paths without a walk.
///
/// For a plain path the walk of [`glob_walk`] says "overlaps" in four ways: the glob
/// matches the path; the glob names a directory above it (its last component has no
/// wildcard); the path may be a directory above what the glob names (the path ends before
/// the glob does, or reaches a `**`); or a `[`, `]`, `{` or `}` in the path is matched as
/// a plain character. One compiled matcher gives the same answer whenever the second and
/// fourth cannot apply and the path is long enough for the third to be decided by a match:
/// the whole glob when it has no `**`, and otherwise the glob cut after its first `**`,
/// since a path that matches up to there overlaps whatever follows. The shapes it does not
/// take (`of` returns none, or `touches` does) keep the walk: a glob whose last component
/// can name an entry without a wildcard, a glob with a `/` inside braces or a class,
/// a piece with a `\\`, an empty alternative or `**` inside braces, a class that can match
/// `/`, a path with a bracket or a brace in it, and a path with fewer components than the
/// glob has up to its first `**`.
/// `the_direct_match_agrees_with_the_walk` holds the two to the same answers.
struct DirectMatch<'a> {
    /// The glob's text before its first wildcard: a path that neither starts with it nor
    /// is the start of it cannot overlap, by any reading.
    prefix: &'a str,
    /// The fewest components a path needs for the matcher to decide.
    need: usize,
    matcher: GlobSet,
}

impl<'a> DirectMatch<'a> {
    fn of(glob: &'a str) -> Option<Self> {
        let pieces = pattern_components(glob);
        if pieces
            .iter()
            .any(|p| p.contains('/') || !direct_piece_ok(p))
            || last_falls_back_to_walk(pieces.last()?)
        {
            return None;
        }
        let (matcher, need) = match pieces.iter().position(|p| *p == "**") {
            Some(at) => {
                let head: String = pieces[..at].iter().map(|p| format!("{p}/")).collect();
                (compile(&format!("{head}**")).ok()?, at + 1)
            }
            None => (compile(glob).ok()?, pieces.len()),
        };
        Some(Self {
            prefix: literal_prefix(glob),
            need,
            matcher,
        })
    }

    /// Whether the glob overlaps a hold on the plain path `path`; none when only the walk
    /// can say.
    fn touches(&self, path: &str) -> Option<bool> {
        if !path.starts_with(self.prefix) && !self.prefix.starts_with(path) {
            return Some(false);
        }
        if path.contains(['[', ']', '{', '}']) || path.split('/').count() < self.need {
            return None;
        }
        Some(self.matcher.is_match(path))
    }
}

/// Compile a pattern, or say why not. Never panics, whatever the pattern: one over
/// [`MAX_HOLD_PATTERN_BYTES`] is refused unread, and the matcher is built as a set of one
/// glob, because globset returns an error from a set when the compiled form passes its own
/// size limit (10 MiB) where `Glob::compile_matcher` panics.
fn compile(pattern: &str) -> Result<GlobSet> {
    crate::queue::within_bytes("hold pattern", pattern, MAX_HOLD_PATTERN_BYTES)?;
    let glob = GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|e| Error::Invalid(format!("invalid glob {pattern:?}: {e}")))?;
    GlobSetBuilder::new().add(glob).build().map_err(|_| {
        Error::Invalid(format!(
            "invalid glob {}: it is too large to compile",
            pattern_brief(pattern)
        ))
    })
}

/// A stored pattern this build cannot use: a glob longer than the limit, or one that does
/// not compile. A pattern without glob syntax is compared as text and never compiled, so
/// it is readable whatever it spells and however long it is; the edit hook's holds are
/// such patterns and are not held to the limit (D99).
pub(crate) fn pattern_unreadable(pattern: &str) -> bool {
    literal_pattern(pattern).is_none() && compile(pattern).is_err()
}

/// Decode an entirely literal pattern, including globset's single-character escapes.
/// Real character classes, wildcards and alternations still return None.
pub(crate) fn literal_pattern(pattern: &str) -> Option<String> {
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
pub(crate) fn hold_covers(held: &str, path: &str) -> bool {
    let path = literal_pattern(path).unwrap_or_else(|| path.to_string());
    if held == path {
        return true;
    }
    match literal_pattern(held) {
        Some(held) => covers(&held, &path),
        // An own glob that cannot be read covers nothing: the edit takes its own narrow
        // hold instead of resting on one nobody can check. One too costly to walk covers
        // only what it matches as a whole, for the same reason.
        None => {
            held == "**"
                || compile(held).is_ok_and(|whole| {
                    let mut budget = WALK_BUDGET;
                    match glob_walk(held, &path, false, &mut budget) {
                        Touch::Yes => true,
                        Touch::No => false,
                        Touch::Unknown => whole.is_match(&path),
                    }
                })
        }
    }
}

/// The part of a pattern before its first glob metacharacter.
pub(crate) fn literal_prefix(pattern: &str) -> &str {
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
    let mut budget = WALK_BUDGET;
    overlap_within(a, b, &mut budget) != Touch::No
}

/// [`patterns_overlap`] within `budget` steps, with "ran out" as its own answer.
fn overlap_within(a: &str, b: &str, budget: &mut usize) -> Touch {
    let yes = |found: bool| if found { Touch::Yes } else { Touch::No };
    if a == b || a == "**" || b == "**" {
        return Touch::Yes;
    }
    // Older auto holds stored filenames without escapes. Keep their exact spelling
    // protected while also interpreting any deliberate manual glob conservatively.
    if literal_pattern(a).as_deref() == Some(b) || literal_pattern(b).as_deref() == Some(a) {
        return Touch::Yes;
    }
    match (literal_pattern(a), literal_pattern(b)) {
        (Some(a), Some(b)) => yes(covers(&a, &b) || covers(&b, &a)),
        (None, Some(b)) => glob_walk(a, &b, true, budget),
        (Some(a), None) => glob_walk(b, &a, true, budget),
        (None, None) => {
            let (pa, pb) = (literal_prefix(a), literal_prefix(b));
            yes(pa.starts_with(pb) || pb.starts_with(pa))
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
pub(crate) fn folds_case(dir: &Path) -> bool {
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

/// The last component keeps the walk when any expansion names an entry without a
/// wildcard: `{core,*-plugin}` spells out `core`, which the walk treats as a directory
/// hold covering everything below it. `*.{ts,tsx}` still has a wildcard in every
/// expansion, so it can use the direct match. Check each part separately rather than
/// expanding every combination of alternatives.
fn last_falls_back_to_walk(last: &str) -> bool {
    let Some((before, alternatives, after)) = first_alternation(last) else {
        return names_entries(last);
    };
    names_entries(before)
        && last_falls_back_to_walk(after)
        && alternatives.iter().any(|a| last_falls_back_to_walk(a))
}

/// Shapes the direct match does not take, so the walk is kept: a backslash, an empty
/// alternative or `**` inside braces, or a character class that can match `/`.
fn direct_piece_ok(piece: &str) -> bool {
    if piece.contains('\\') || class_can_match_slash(piece) {
        return false;
    }
    let Some((_, alts, after)) = first_alternation(piece) else {
        return true;
    };
    if alts.iter().any(|a| a.is_empty() || a.contains("**")) {
        return false;
    }
    alts.iter().all(|a| direct_piece_ok(a)) && direct_piece_ok(after)
}

/// A class matches `/` when it contains one, or when it is negated (`[!` / `[^`) and so
/// can. An unclosed `[` is a plain character and does not.
fn class_can_match_slash(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < pattern.len() {
        if bytes[i] == b'[' {
            if let Some(end) = class_end(pattern, i) {
                let mut inner = i + 1;
                let negated = matches!(bytes.get(inner), Some(b'!' | b'^'));
                if negated {
                    inner += 1;
                }
                if negated || pattern[inner..end].contains('/') {
                    return true;
                }
                i = end + 1;
                continue;
            }
        }
        i += pattern[i..]
            .chars()
            .next()
            .map(|c| c.len_utf8())
            .unwrap_or(1);
    }
    false
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
/// such as `{a/b,c}` stays one piece until `components_touch` reaches it.
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

/// Steps one comparison may take. One that runs past it counts as an overlap between
/// holds of this node: a spurious conflict costs a retry, a missed one lets two agents
/// edit one file. A step costs about 8 microseconds in a release build (it compiles one
/// component, twice at most); the longest single comparison measured, over the hostile
/// shapes of both reviews, took 21 ms, so 25 ms is a bound with some margin. The
/// patterns in this repository's tests take at most 37 steps and the widest honest one
/// measured takes 493 (`honest_patterns_stay_far_inside_the_walk_budget`); only a pattern
/// that branches at every component, such as `{a,b}` fourteen times over, reaches this.
/// It was 20,000 until the review of Q30 measured 0.2 s for one comparison of an 86 byte
/// glob.
const WALK_BUDGET: usize = 2_000;

/// What one command may spend on comparing with the holds of other nodes, all of them
/// together, in estimated microseconds of a release build. Those holds never block, so
/// the comparison is best effort: when the budget is spent the rest are not compared and
/// the command says how many, from which nodes. The estimates, from measurements on this
/// project's development machine: reading a hold costs 1 and one more for every
/// [`READ_BYTES_PER_MICRO`] bytes of its pattern, comparing two patterns as text or by a
/// direct match costs 1 and one more for every [`TEXT_BYTES_PER_MICRO`] bytes, compiling a
/// glob to learn whether it can be read costs half a microsecond a byte, and a walk costs
/// [`STEP_MICROS`] a step, [`WALK_BUDGET`] steps at most. What that buys is pinned by
/// `what_one_command_compares_of_a_peers_holds`: 19,999 holds on plain paths, whether a
/// path or a glob is asked for, and a few hundred to a few thousand glob holds. Three
/// globs built to be costly use all of it. Measured against 10,000 holds of each hostile
/// shape tried, a whole `hold acquire` takes about 50 ms at most (docs/DECISIONS.md, the
/// Q30 row).
pub(crate) const FOREIGN_BUDGET: usize = 40_000;

/// The estimated cost of one walk step, in microseconds.
const STEP_MICROS: usize = 8;

/// Bytes of pattern text compared as text in one estimated microsecond.
const TEXT_BYTES_PER_MICRO: usize = 128;

/// Bytes of a stored pattern read from the database in one estimated microsecond: a 4 KB
/// pattern was measured at about 5 microseconds to read and tell a glob from a path.
const READ_BYTES_PER_MICRO: usize = 1024;

/// Overlapping holds of other nodes one command lists one by one; the rest are counted.
pub(crate) const FOREIGN_LISTED: usize = 10;

/// Compiling a component is what a step costs, and a long component costs more: every
/// this many bytes of a compiled component take one more step from the budget. A
/// component shorter than this (nearly every real file or directory name pattern) costs
/// nothing extra, so realistic patterns are judged exactly as before. Without it a pattern
/// inside the size limit, a 4,000 byte component behind a dozen alternations, was compiled
/// thousands of times in one comparison, which took minutes.
const COMPILE_BYTES_PER_STEP: usize = 16;

/// Glob components against the components of a literal path. An alternation in the first
/// component is spelled out one alternative at a time, each respliced into components, so
/// `{a/b,c}` continues as `a`, `b` or as `c` and no chain of alternations is ever expanded
/// in full. `**` spans zero or more components. A glob whose last component names an
/// entry (`names_entries`) and that ends above the rest of the path is a directory hold,
/// and covers everything below it; `src/*` names the entries directly in `src` and nothing
/// deeper. A path that ends first is an ancestor directory of what the glob names when the
/// caller allows the path to be a directory (`path_may_be_dir`); an edited file is never
/// that. Every call spends one step of `budget`; at zero the answer is an overlap.
fn components_touch(
    glob: &[&str],
    path: &[&str],
    path_may_be_dir: bool,
    budget: &mut usize,
) -> bool {
    if *budget == 0 {
        return true;
    }
    *budget -= 1;
    if let Some((g, rest)) = glob.split_first() {
        if let Some((head, alternatives, tail)) = first_alternation(g) {
            return alternatives.iter().any(|alt| {
                let spliced = format!("{head}{alt}{tail}");
                let mut pieces = pattern_components(&spliced);
                pieces.extend_from_slice(rest);
                components_touch(&pieces, path, path_may_be_dir, budget)
            });
        }
    }
    match (glob.split_first(), path.split_first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some(_), None) => path_may_be_dir,
        (Some((&"**", rest)), Some((_, deeper))) => {
            rest.is_empty()
                || components_touch(rest, path, path_may_be_dir, budget)
                || components_touch(glob, deeper, path_may_be_dir, budget)
        }
        (Some((g, rest)), Some((p, deeper))) => {
            *budget = budget.saturating_sub(g.len() / COMPILE_BYTES_PER_STEP);
            component_touches(g, p)
                && if rest.is_empty() {
                    deeper.is_empty() || names_entries(g)
                } else {
                    components_touch(rest, deeper, path_may_be_dir, budget)
                }
        }
    }
}

/// A glob against a literal path: component by component with alternations spelled out
/// as they are reached, and the whole as globset reads it.
///
/// The answer is `Unknown` when `budget` ran out. Each caller says what that means: an
/// overlap where a missed one would let two agents edit one file, "does not cover" for an
/// agent's own hold, "not compared" for another node's hold.
fn glob_walk(glob: &str, literal: &str, path_may_be_dir: bool, budget: &mut usize) -> Touch {
    // A glob over the limit is not walked or compiled at all (a peer or an older build
    // can have stored megabytes): it overlaps everything, at no cost.
    if glob.len() > MAX_HOLD_PATTERN_BYTES {
        return Touch::Yes;
    }
    let lit: Vec<&str> = literal.split('/').collect();
    let touched = components_touch(&pattern_components(glob), &lit, path_may_be_dir, budget);
    // The whole-glob reading compiles the glob once more, and pays for it.
    *budget = budget.saturating_sub(1 + glob.len() / COMPILE_BYTES_PER_STEP);
    if *budget == 0 {
        Touch::Unknown
    } else if touched || matches(glob, literal) {
        Touch::Yes
    } else {
        Touch::No
    }
}

/// What a bounded comparison found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Touch {
    Yes,
    No,
    /// The budget ran out before an answer.
    Unknown,
}

/// Link local projects that hold live holds but have no current repository link: never
/// linked (registered before links existed), linked under older key rules (see
/// `repo::KEY_VERSION`), or linked before the checkout was stored. Their holds then count
/// in their checkout straight away.
pub(crate) fn link_unlinked(tx: &Tx<'_>) -> Result<()> {
    let mut stmt = tx.prepare(
        "SELECT DISTINCT p.project_id, p.node_id, p.abs_path, p.slug, p.created_at
         FROM holds h JOIN projects p ON p.project_id = h.project_id
         LEFT JOIN project_repos pr ON pr.project_id = p.project_id
         WHERE h.released_at IS NULL AND h.expires_at > ?1 AND p.node_id = ?2
           AND (pr.project_id IS NULL
                OR (pr.repo_key != '' AND (substr(pr.repo_key, 1, length(?3)) != ?3
                                           OR pr.checkout = '')))",
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
    let (checkout, my_prefix) = crate::repo::link_of(tx, project)?
        .map_or((String::new(), String::new()), |l| (l.checkout, l.prefix));
    // This project's holds, plus holds in any local project of the same checkout: one
    // registered at another directory of it. Worktrees and clones of the repository are
    // other checkouts and are never candidates (D92).
    let mut stmt = tx.prepare(&format!(
        "{HOLD_SELECT_REPO} WHERE h.released_at IS NULL AND h.expires_at > ?3
           AND h.agent_id != ?2
           AND (NOT EXISTS (SELECT 1 FROM contract_actors a WHERE a.actor_id = ?2)
                OR EXISTS (SELECT 1 FROM contract_actors a WHERE a.actor_id = h.agent_id))
           AND (h.project_id = ?1 OR (?4 != '' AND h.project_id IN
                (SELECT project_id FROM project_repos WHERE checkout = ?4)))
         ORDER BY h.created_at"
    ))?;
    let others = stmt
        .query_map(
            params![project.project_id, agent_id, tx.now, checkout],
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
    // A stored pattern that cannot be read (a row an older build wrote) fails closed: it
    // stands for every path of the project it is in, and for nothing outside that project.
    let unreadable: Vec<bool> = others
        .iter()
        .map(|(held, _, _)| pattern_unreadable(&held.pattern))
        .collect();
    let mut conflicts = Vec::new();
    for (requested, pattern) in patterns {
        for ((held, held_project, held_prefix), unreadable) in others.iter().zip(&unreadable) {
            if !(exclusive || held.exclusive) {
                continue;
            }
            let theirs = if *unreadable { "**" } else { &held.pattern };
            let overlap = if *held_project == project.project_id {
                overlaps(pattern, theirs)
            } else {
                // Compare as paths from the top of the checkout.
                overlaps(
                    &crate::repo::in_repo(&my_prefix, pattern),
                    &crate::repo::in_repo(held_prefix, theirs),
                )
            };
            if overlap && *unreadable {
                conflicts.push(unreadable_conflict(tx, tx.node_id, requested, held)?);
            } else if overlap {
                conflicts.push(HoldConflict {
                    path: requested.clone(),
                    held: held.clone(),
                    unreadable: None,
                });
            }
        }
    }
    Ok(conflicts)
}

/// The conflict a hold makes when its stored pattern cannot be read. The note names the
/// holder's node and says how to clear the hold; the pattern is shown as a short prefix
/// and a length, never whole.
fn unreadable_conflict(
    conn: &Connection,
    local: &str,
    requested: &str,
    held: &Hold,
) -> Result<HoldConflict> {
    let node: Option<(String, bool)> = conn
        .query_row(
            "SELECT n.display_name, n.node_id = ?2 FROM holds h
             JOIN nodes n ON n.node_id = h.node_id WHERE h.hold_id = ?1",
            params![held.hold_id, local],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let brief = pattern_brief(&held.pattern);
    let (id, agent, project) = (&held.hold_id, &held.agent, &held.project);
    let note = match node {
        Some((name, false)) => format!(
            "hold {id} of {agent} on node {name} has a pattern that could not be read \
             ({brief}), so it counts as overlapping every path in project {project}. It ends \
             at {} unless that node renews it; to clear it sooner, release hold {id} on node \
             {name} and sync",
            held.expires_at
        ),
        Some((name, true)) => format!(
            "hold {id} of {agent} on this node ({name}) has a pattern that could not be read \
             ({brief}), so it counts as overlapping every path in project {project}. To clear \
             it: another hold release --agent {agent} {id}"
        ),
        None => format!(
            "hold {id} of {agent} has a pattern that could not be read ({brief}), so it counts \
             as overlapping every path in project {project}"
        ),
    };
    Ok(HoldConflict {
        path: requested.to_string(),
        held: Hold {
            pattern: brief,
            ..held.clone()
        },
        unreadable: Some(crate::mail::display_line(&note, 700)),
    })
}

/// Create a hold, or return the actor's existing live checkout unchanged.
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
               AND pattern = ?3 AND released_at IS NULL AND expires_at > ?4",
            params![agent_id, project_id, pattern, tx.now],
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
        Some(id) => id,
        None => {
            let id = new_id();
            let root: String = tx.query_row(
                "SELECT abs_path FROM projects WHERE project_id = ?1",
                [project_id],
                |r| r.get(0),
            )?;
            let literal = literal_pattern(pattern);
            if literal.is_none()
                && tx
                    .query_row(
                        "SELECT 1 FROM contract_actors WHERE actor_id = ?1",
                        [agent_id],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some()
            {
                return Err(Error::Invalid(
                    "contract holds require exact file targets".into(),
                ));
            }
            let target = crate::repo::resolve_existing(
                &Path::new(&root).join(literal.as_deref().unwrap_or(pattern)),
            );
            crate::actor_leases::projection_checkout(
                tx,
                agent_id,
                crate::SubjectKind::File,
                &target.to_string_lossy(),
                ttl,
                &id,
            )?;
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

/// What one command found when it compared with the holds of other nodes.
#[derive(Debug, Default)]
pub(crate) struct ForeignClashes {
    /// The first [`FOREIGN_LISTED`] overlaps: the index of the local pattern, the clash and
    /// the name of the node that holds it.
    pub listed: Vec<(usize, HoldConflict, String)>,
    /// Overlaps found after those.
    pub more: u32,
    /// Holds that were not compared, by node, in node name order.
    pub not_compared: Vec<NotCompared>,
}

/// Compare `patterns` with the live holds other nodes have in a project called `slug`,
/// and add what is found to `found`.
///
/// Best effort and bounded, because those holds never block and a peer chooses how many
/// there are and what they cost: every hold read and every step of every comparison comes
/// out of `budget`, which the caller sets once for the whole command. When it is spent, or
/// one comparison runs past [`WALK_BUDGET`], the hold is not compared; such holds are
/// counted by node from the index alone. The holds are read one at a time, in no promised
/// order, so nothing a peer sent is loaded that is not compared. Reads only: it runs
/// outside the write transaction.
///
/// A peer's pattern that cannot be read (a row stored before patterns were checked on
/// arrival) fails closed: it counts as overlapping every path in this project.
pub(crate) fn foreign_clashes(
    conn: &Connection,
    local: &str,
    now: i64,
    slug: &str,
    patterns: &[(String, String)],
    budget: &mut usize,
    found: &mut ForeignClashes,
) -> Result<()> {
    // Take `cost` from the budget; false when that spends it.
    let spend = |budget: &mut usize, cost: usize| {
        *budget = budget.saturating_sub(cost);
        *budget > 0
    };
    // What can be known about each requested glob before any hold is read: compiled once
    // here and paid for, it judges a hold on a plain path without a walk.
    let direct: Vec<Option<DirectMatch>> = patterns
        .iter()
        .map(|(_, pattern)| {
            let glob = literal_pattern(pattern).is_none();
            (glob && spend(budget, pattern.len().min(MAX_HOLD_PATTERN_BYTES) / 2))
                .then(|| DirectMatch::of(pattern))
                .flatten()
        })
        .collect();
    let mut compared: std::collections::BTreeMap<String, u32> = Default::default();
    let mut all_compared = true;
    // Judge one hold whose row has been read and paid for. False when the budget, or one
    // comparison's own limit, ran out before an answer.
    let mut judge = |held: &Hold,
                     node: &str,
                     literal: Option<&str>,
                     budget: &mut usize,
                     found: &mut ForeignClashes|
     -> Result<bool> {
        let size = held.pattern.len();
        // A glob is compiled once to learn whether it can be read.
        let mut known = literal.is_some() || spend(budget, size.min(MAX_HOLD_PATTERN_BYTES) / 2);
        let unreadable = known && literal.is_none() && compile(&held.pattern).is_err();
        // The local patterns this hold overlaps, by index.
        let mut clashes = Vec::new();
        if unreadable {
            clashes.extend(0..patterns.len());
        } else if known {
            for (i, (_, pattern)) in patterns.iter().enumerate() {
                // Comparing the two as text.
                if !spend(budget, 1 + (size + pattern.len()) / TEXT_BYTES_PER_MICRO) {
                    known = false;
                    break;
                }
                // A requested glob against a hold on a plain path: matched directly where
                // that is the same answer as the walk, at the cost already paid.
                let quick = match (&direct[i], literal) {
                    (Some(direct), Some(path)) => direct.touches(path),
                    _ => None,
                };
                let touch = match quick {
                    Some(true) => Touch::Yes,
                    Some(false) => Touch::No,
                    None => {
                        let allowed = (*budget / STEP_MICROS).min(WALK_BUDGET);
                        let mut steps = allowed;
                        let touch = overlap_within(pattern, &held.pattern, &mut steps);
                        *budget -= (allowed - steps) * STEP_MICROS;
                        touch
                    }
                };
                match touch {
                    Touch::Yes => clashes.push(i),
                    Touch::No => {}
                    Touch::Unknown => {
                        known = false;
                        break;
                    }
                }
            }
        }
        if !known {
            return Ok(false);
        }
        *compared.entry(node.to_string()).or_default() += 1;
        for i in clashes {
            // Only a clash that is listed is built: the rest are a number.
            if found.listed.len() >= FOREIGN_LISTED {
                found.more += 1;
                continue;
            }
            let requested = &patterns[i].0;
            let clash = if unreadable {
                unreadable_conflict(conn, local, requested, held)?
            } else {
                let mut held = held.clone();
                // A long pattern is shown as its start and its length: ten warnings with
                // a peer's 4 KB patterns whole were 38 KB of output.
                if held.pattern.chars().nth(PATTERN_BRIEF_CHARS).is_some() {
                    held.pattern = pattern_brief(&held.pattern);
                }
                HoldConflict {
                    path: requested.clone(),
                    held,
                    unreadable: None,
                }
            };
            found.listed.push((i, clash, node.to_string()));
        }
        Ok(true)
    };
    // Holds on plain paths are compared as they are read; globs are set aside and compared
    // after them. The plain ones are what an honest peer's edits make and cost next to
    // nothing, so no glob's compile or walk is paid for before they have been compared.
    // Reading a hold is itself paid for, so what is set aside is bounded by the budget.
    let mut globs: Vec<(Hold, String)> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT h.hold_id, p.slug, a.address, NULL, h.pattern, h.exclusive, h.reason,
               h.ttl_seconds, h.created_at, h.expires_at, n.display_name
             FROM holds h
             JOIN projects p ON p.project_id = h.project_id
             JOIN agents a ON a.agent_id = h.agent_id
             JOIN nodes n ON n.node_id = p.node_id
             WHERE h.released_at IS NULL AND h.expires_at > ?1 AND p.node_id != ?2
               AND p.slug = ?3",
        )?;
        let mut rows = stmt.query(params![now, local, slug])?;
        while let Some(row) = rows.next()? {
            if *budget == 0 {
                all_compared = false;
                break;
            }
            let held = hold_from_row(row)?;
            let node: String = row.get(10)?;
            if !spend(budget, 1 + held.pattern.len() / READ_BYTES_PER_MICRO) {
                all_compared = false;
                break;
            }
            match literal_pattern(&held.pattern) {
                Some(path) => all_compared &= judge(&held, &node, Some(&path), budget, found)?,
                None => globs.push((held, node)),
            }
        }
    }
    for (held, node) in &globs {
        all_compared &= *budget > 0 && judge(held, node, None, budget, found)?;
    }
    if all_compared {
        return Ok(());
    }
    // How many there are, by node, from the index alone: no pattern is read for this.
    let mut stmt = conn.prepare(
        "SELECT n.display_name, count(*) FROM holds h
         JOIN projects p ON p.project_id = h.project_id
         JOIN nodes n ON n.node_id = p.node_id
         WHERE h.released_at IS NULL AND h.expires_at > ?1 AND p.node_id != ?2 AND p.slug = ?3
         GROUP BY n.node_id ORDER BY n.display_name",
    )?;
    let totals = stmt
        .query_map(params![now, local, slug], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (node, total) in totals {
        let left = total.saturating_sub(compared.get(&node).copied().unwrap_or(0));
        if left == 0 {
            continue;
        }
        match found.not_compared.iter_mut().find(|n| n.node == node) {
            Some(n) => n.holds += left,
            None => found.not_compared.push(NotCompared { node, holds: left }),
        }
    }
    Ok(())
}

/// Why a peer's `hold.acquired` or `hold.updated` is not applied as a hold: its pattern is
/// longer than a local `hold acquire` accepts. None for every other event. The event
/// itself stays stored and relayed, so sync goes on; the refusal is written to the audit
/// log with the origin node and the event id, and the returned text is what `another sync`
/// prints. The pattern is never quoted whole.
///
/// Only the length is judged here, and nothing is compiled: this runs inside the ingest
/// write transaction, once for every event of a batch, and a glob of this size takes 2 ms
/// to compile. Whether a pattern within the limit compiles is found out when it is first
/// compared ([`foreign_clashes`]), inside that command's budget and outside any write.
pub(crate) fn refuse_foreign_pattern(
    tx: &Tx<'_>,
    origin: &str,
    event_id: &str,
    kind: &str,
    payload: &Value,
) -> Result<Option<String>> {
    if !matches!(kind, "hold.acquired" | "hold.updated") {
        return Ok(None);
    }
    let Some(pattern) = payload.get("pattern").and_then(Value::as_str) else {
        return Ok(None);
    };
    if pattern.len() <= MAX_HOLD_PATTERN_BYTES {
        return Ok(None);
    }
    let name: Option<String> = tx
        .query_row(
            "SELECT display_name FROM nodes WHERE node_id = ?1",
            [origin],
            |r| r.get(0),
        )
        .optional()?;
    let reason = crate::mail::display_line(
        &format!(
            "{kind} from node {} ({origin}) was stored and relayed but not applied as a hold: \
             its pattern {} is longer than the limit of {MAX_HOLD_PATTERN_BYTES} bytes",
            name.as_deref().unwrap_or("unknown"),
            pattern_brief(pattern)
        ),
        700,
    );
    tx.execute(
        "INSERT INTO audit_log (node_id, ts, token_id, agent, operation, outcome, detail, remote)
         VALUES (?1, ?2, NULL, ?3, 'federation hold pattern', 'refused', ?4, NULL)",
        params![
            tx.node_id,
            tx.now,
            format!("node:{origin}"),
            format!("event {event_id}: {reason}")
        ],
    )?;
    Ok(Some(reason))
}

/// The blocked paths and, per path, the sorted ids of the holds blocking it. This is the
/// identity of a wait: the same path with a different set of hold ids is a new conflict.
fn blocker_sets(conflicts: &[HoldConflict]) -> Vec<(&str, Vec<&str>)> {
    let mut paths: Vec<&str> = conflicts.iter().map(|c| c.path.as_str()).collect();
    paths.sort_unstable();
    paths.dedup();
    paths
        .into_iter()
        .map(|path| {
            let mut blockers: Vec<&str> = conflicts
                .iter()
                .filter(|c| c.path == path)
                .map(|c| c.held.hold_id.as_str())
                .collect();
            blockers.sort_unstable();
            blockers.dedup();
            (path, blockers)
        })
        .collect()
}

/// Register durable file waits and advance backoff only after its current window ends.
/// Returns whether any blocker set is new, and the longest remaining backoff.
fn wait_for_conflicts_tx(
    tx: &Tx<'_>,
    agent: &crate::identity::AgentRef,
    project: &Project,
    conflicts: &[HoldConflict],
) -> Result<(bool, i64)> {
    let mut first_notice = false;
    let mut retry_after = 0;
    for (path, blockers) in blocker_sets(conflicts) {
        let signature = serde_json::to_string(&blockers)?;
        let old: Option<(String, i64, i64)> = tx
            .query_row(
                "SELECT blockers, retry_at, backoff_seconds FROM hold_waits
             WHERE agent_id = ?1 AND project_id = ?2 AND path = ?3",
                params![agent.id, project.project_id, path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let is_new = old.is_none()
            || recorded_wait_is_new(tx, &agent.id, &project.project_id, path, &blockers)?;
        let (retry_at, backoff) = match old {
            Some((stored, until, backoff)) if !is_new && stored == signature && until > tx.now => {
                (until, backoff)
            }
            old => {
                let (until, backoff) = match old {
                    Some((_, until, previous)) if !is_new && until > tx.now => (until, previous),
                    Some((_, _, previous)) if !is_new => {
                        let backoff = (previous * 2).min(300);
                        (tx.now + backoff * 1000, backoff)
                    }
                    _ => (tx.now + 30_000, 30),
                };
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

/// A refreshed row is bookkeeping; only the last successful wait event records which
/// blockers were announced and whose backoff applies. Terminal events end that record.
fn recorded_wait_is_new(
    tx: &Tx<'_>,
    agent_id: &str,
    project_id: &str,
    path: &str,
    blockers: &[&str],
) -> Result<bool> {
    let recorded: Option<(String, String)> = tx
        .query_row(
            "SELECT kind, payload FROM events
             WHERE kind IN ('hold.waiting', 'hold.available', 'hold.notify_failed', 'hold.wait.finished')
               AND json_extract(payload, '$.agent_id') = ?1
               AND json_extract(payload, '$.project_id') = ?2
               AND json_extract(payload, '$.path') = ?3
             ORDER BY seq DESC LIMIT 1",
            params![agent_id, project_id, path],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let announced = |payload: &str| {
        serde_json::from_str::<Value>(payload)
            .ok()
            .and_then(|v| serde_json::from_value::<Vec<String>>(v["blockers"].clone()).ok())
    };
    Ok(!recorded.as_ref().is_some_and(|(kind, payload)| {
        kind == "hold.waiting" && announced(payload).is_some_and(|stored| stored == blockers)
    }))
}

/// A failed update may have announced another set without replacing the successful
/// one. Either announcement suppresses a duplicate denial until the wait ends.
fn failed_wait_is_new(
    tx: &Tx<'_>,
    agent_id: &str,
    project_id: &str,
    path: &str,
    blockers: &[&str],
) -> Result<bool> {
    if !recorded_wait_is_new(tx, agent_id, project_id, path, blockers)? {
        return Ok(false);
    }
    let latest: Option<(String, String)> = tx
        .query_row(
            "SELECT kind, payload FROM events
             WHERE kind IN ('hold.waiting', 'hold.wait_failed', 'hold.available',
                            'hold.notify_failed', 'hold.wait.finished')
               AND json_extract(payload, '$.agent_id') = ?1
               AND json_extract(payload, '$.project_id') = ?2
               AND json_extract(payload, '$.path') = ?3
             ORDER BY seq DESC LIMIT 1",
            params![agent_id, project_id, path],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(match latest {
        Some((kind, payload)) if kind == "hold.waiting" || kind == "hold.wait_failed" => {
            serde_json::from_str::<Value>(&payload)
                .ok()
                .and_then(|v| serde_json::from_value::<Vec<String>>(v["blockers"].clone()).ok())
                .is_none_or(|stored| stored != blockers)
        }
        _ => true,
    })
}

/// Run `f` inside a savepoint. On error only its own work is rolled back and the error is
/// returned; the surrounding transaction stays usable. The event chain is unaffected, since
/// each emit reads its predecessor from the table.
pub(crate) fn attempt<T>(tx: &Tx<'_>, name: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
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
/// current blockers in the file's checkout, including replacement holds. Waiter and
/// holder are addressed by the ids the wait row stores. A ping that cannot be delivered is
/// recorded as `hold.notify_failed` and its wait is dropped anyway, so a release or a sweep
/// never fails, and never fails again later, because of one notice.
/// A wait row: waiter id, project slug, project id, path, holder id, blocker ids (JSON).
type WaitRow = (String, String, String, String, String, String);

/// The waits that can have become free: those with at least one stored blocker that is no
/// longer live (released, past its expiry whether swept or not, or unknown, on this node or
/// another). A live hold never changes its pattern, project or owner in place, so a wait
/// whose every blocker is still live still conflicts and needs no recheck; this keeps
/// writes that end no hold from rechecking every wait (#137 adversarial F1). A row whose
/// blockers are not a non-empty JSON array is always rechecked.
fn waits_to_recheck_tx(tx: &Tx<'_>) -> Result<Vec<WaitRow>> {
    let mut stmt = tx.prepare(
        "SELECT w.agent_id, p.slug, w.project_id, w.path, w.holder_id, w.blockers
         FROM hold_waits w JOIN projects p ON p.project_id = w.project_id
         WHERE CASE WHEN json_valid(w.blockers) AND json_type(w.blockers) = 'array'
                    THEN json_array_length(w.blockers) = 0
                      OR EXISTS (SELECT 1 FROM json_each(w.blockers) b
                                 WHERE NOT EXISTS (SELECT 1 FROM holds h
                                   WHERE h.hold_id = b.value AND h.released_at IS NULL
                                     AND h.expires_at > ?1))
                    ELSE 1 END",
    )?;
    let waits = stmt
        .query_map([tx.now], |r| {
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
    Ok(waits)
}

fn ping_free_waiters_tx(tx: &Tx<'_>) -> Result<()> {
    for (agent_id, project_key, project_id, path, holder_id, blockers) in waits_to_recheck_tx(tx)? {
        let project = find_project(tx, tx.node_id, &project_key)?;
        let conflicts = conflicts_for(
            tx,
            &project,
            &agent_id,
            &[(path.clone(), globset::escape(&path))],
            true,
        )?;
        if !conflicts.is_empty() {
            let mut current: Vec<&str> =
                conflicts.iter().map(|c| c.held.hold_id.as_str()).collect();
            current.sort_unstable();
            current.dedup();
            // Retire processed ended IDs and capture replacement holds. Keep the original
            // notice sender, retry deadline and backoff; this is not a waiter retry.
            let refreshed = attempt(tx, "wait_refresh", || {
                tx.emit(
                    "system",
                    "hold.wait.rechecked",
                    json!({
                        "agent_id": agent_id, "project_id": project_id,
                        "path": path, "blockers": current,
                    }),
                )?;
                tx.execute(
                    "UPDATE hold_waits SET blockers = ?4
                     WHERE agent_id = ?1 AND project_id = ?2 AND path = ?3",
                    params![agent_id, project_id, path, serde_json::to_string(&current)?],
                )?;
                Ok(())
            });
            if let Err(error) = refreshed {
                tx.emit(
                    "system",
                    "hold.recheck_failed",
                    json!({
                        "agent_id": agent_id, "project_id": project_id,
                        "path": path, "blockers": current, "error": error.to_string(),
                    }),
                )?;
            }
            continue;
        }
        // Naming the ended holds makes every ping distinct, so the push dedupe never
        // swallows a second ping for the same file within its five-minute window.
        let ended: Vec<String> = serde_json::from_str(&blockers).unwrap_or_default();
        // A wait with no blocker on record has no ended hold to name: one emptied by the
        // migration that scoped holds to a checkout, whose blocker was in another one.
        let why = if ended.is_empty() {
            "no conflicting hold remains in this checkout".to_string()
        } else {
            format!(
                "no conflicting hold remains (hold {} ended)",
                ended.join(", ")
            )
        };
        let notice = SendMessage {
            // A file name may contain a line break; subjects are one line (D75).
            subject: format!("Hold free: {}", crate::mail::single_line(&path, 280)),
            body: format!(
                "Another automatically checked {project_key}:{path}: {why}. The file is not reserved; check again before editing."
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
            // The title may come from a subject stored before D75.
            subject: format!("Done: {}", crate::mail::single_line(&title, 290)),
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
    crate::requests::remind_due_soon_tx(tx)?;
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
    for ids in hold_ids.chunks(HOLD_IDS_PER_EVENT) {
        tx.emit(
            actor,
            "hold.released",
            json!({ "hold_ids": ids, "reason": reason }),
        )?;
    }
    for id in hold_ids {
        crate::actor_leases::projection_release(tx, id, reason)?;
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
    for chunk in holds.chunks(HOLD_IDS_PER_EVENT) {
        let ids: Vec<&String> = chunk.iter().map(|(id, _)| id).collect();
        tx.emit(actor, "hold.renewed", json!({ "hold_ids": ids }))?;
    }
    for (id, ttl) in holds {
        crate::actor_leases::projection_renew(tx, id)?;
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
            let me = crate::identity::resolve_acting_agent(tx, tx.node_id, agent)?;
            let held = tx.query_row(&format!("{HOLD_SELECT} WHERE h.hold_id = ?1
                AND h.node_id = ?2 AND h.released_at IS NULL AND h.expires_at > ?3"),
                params![hold_id, tx.node_id, tx.now], hold_from_row).optional()?
                .ok_or_else(|| Error::NotFound(format!("unknown live local hold {hold_id:?}")))?;
            // The hold names its owner by id; never resolve the owner's display address
            // again, which a later same-named agent can have made ambiguous.
            let holder_id: String = tx.query_row(
                "SELECT agent_id FROM holds WHERE hold_id = ?1", [hold_id], |r| r.get(0))?;
            if holder_id == me.id {
                return Err(Error::Invalid("release your own hold with hold release".into()));
            }
            let existing: Option<String> = tx.query_row(
                "SELECT r.message_id FROM request_holds rh JOIN requests r ON r.request_id = rh.request_id
                 WHERE rh.hold_id = ?1 AND r.created_by = ?2 AND r.status IN ('open', 'claimed', 'stale')",
                params![hold_id, me.id], |r| r.get(0)).optional()?;
            if let Some(mid) = existing { return load_message(tx, &mid); }
            // A pattern that cannot be read is named by its prefix and length: whole, it can
            // be megabytes, which no message body takes.
            let pattern = if pattern_unreadable(&held.pattern) {
                pattern_brief(&held.pattern)
            } else {
                literal_pattern(&held.pattern).unwrap_or(held.pattern)
            };
            // A held file name may contain a line break; subjects are one line (D75).
            let title = crate::mail::single_line(&pattern, 280);
            let message = send_by_id_tx(tx, &me.id, &holder_id, &SendMessage {
                subject: format!("Release {title}?"),
                // The id form of the command: it needs no quoting and no particular directory.
                body: format!("{} is waiting for {}:{pattern}. If you are done with it, free it now: another hold release --agent {} {hold_id}. Another closes this request when the hold ends.", me.address, held.project, held.agent),
                request: Some(NewRequest::default()), ..Default::default()
            }, true)?;
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
        self.acquire_holds_scoped(input, None)
    }

    /// [`Store::acquire_holds`] for a caller that may be remote (`scope`). A pattern that
    /// points into another checkout is refused either way, since it would apply to nothing
    /// (D92); a remote caller's refusal names no absolute path.
    pub fn acquire_holds_scoped(
        &self,
        input: AcquireHolds,
        scope: Option<&TokenScope>,
    ) -> Result<HoldReport> {
        if input.paths.is_empty() {
            return Err(Error::Invalid("no paths given".into()));
        }
        // A reason is printed by `another holds`, the board and the edit guard's denial.
        if let Some(reason) = &input.reason {
            crate::queue::within_bytes("hold reason", reason, MAX_HOLD_REASON_BYTES)?;
            crate::queue::no_unsafe("a hold reason", reason, false)?;
        }
        for raw in &input.paths {
            crate::queue::within_bytes("hold pattern", raw, MAX_HOLD_PATTERN_BYTES)?;
        }
        let ttl = clamp_ttl(input.ttl_seconds)?;
        let (mut report, slug, patterns) = self.write(|tx| {
            sweep_tx(tx)?;
            let me = crate::identity::resolve_acting_agent(tx, tx.node_id, &input.agent)?;
            let project = find_project(tx, tx.node_id, &input.project)?;
            let mut patterns: Vec<(String, String)> = Vec::new();
            for raw in &input.paths {
                let p = normalize_pattern(&project, raw)?;
                if is_repo_wide(&p) && !input.repo_wide {
                    return Err(Error::Invalid(format!(
                        "{raw:?} would hold the whole project ({p}) and block every other agent in \
                         the checkout; hold the files or directories you will change, such as \
                         src/api, or pass --repo-wide (MCP: repo_wide) if you really mean all of it"
                    )));
                }
                crate::checkout::refuse_other_checkout(
                    tx,
                    &project,
                    &me.address,
                    raw,
                    &p,
                    scope.is_some(),
                )?;
                if !patterns.iter().any(|(_, q)| *q == p) {
                    patterns.push((raw.clone(), p));
                }
            }
            let conflicts = conflicts_for(tx, &project, &me.id, &patterns, input.exclusive)?;
            let report = |granted, conflicts| HoldReport {
                granted,
                conflicts,
                warnings: Vec::new(),
                warnings_more: 0,
                not_compared: Vec::new(),
            };
            if !conflicts.is_empty() {
                touch(tx, &me.id)?;
                return Ok((report(Vec::new(), conflicts), project.slug, patterns));
            }
            let mut granted_ids = Vec::new();
            for (_, pattern) in &patterns {
                // Reusing a live hold preserves its selected duration and deadline.
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
            let granted = holds_by_id(tx, &granted_ids)?;
            Ok((report(granted, Vec::new()), project.slug, patterns))
        })?;
        // Other nodes' holds only ever add warnings, so they are compared after the write
        // transaction has committed: comparing with what a peer sent never happens while
        // this command holds the database's write lock.
        let mut found = ForeignClashes::default();
        let mut budget = FOREIGN_BUDGET;
        self.read(|conn| {
            foreign_clashes(
                conn,
                self.node_id(),
                self.now(),
                &slug,
                &patterns,
                &mut budget,
                &mut found,
            )
        })?;
        report.warnings = found.listed.into_iter().map(|(_, c, _)| c).collect();
        report.warnings_more = found.more;
        report.not_compared = found.not_compared;
        Ok(report)
    }

    /// Report which of `paths` would conflict for `agent` (no holds are taken).
    pub fn check_holds(
        &self,
        agent: &str,
        project: &str,
        paths: &[String],
        exclusive: bool,
    ) -> Result<Vec<HoldConflict>> {
        self.check_holds_scoped(agent, project, paths, exclusive, None)
    }

    /// [`Store::check_holds`] for a caller that may be remote (`scope`). A pattern that
    /// points into another checkout is refused as for [`Store::acquire_holds_scoped`]:
    /// "no conflicts" would be a wrong answer for files this project's holds never reach.
    pub fn check_holds_scoped(
        &self,
        agent: &str,
        project: &str,
        paths: &[String],
        exclusive: bool,
        scope: Option<&TokenScope>,
    ) -> Result<Vec<HoldConflict>> {
        self.write(|tx| {
            sweep_tx(tx)?;
            let me = crate::identity::resolve_acting_agent(tx, tx.node_id, agent)?;
            let project = find_project(tx, tx.node_id, project)?;
            let patterns = paths
                .iter()
                .map(|raw| {
                    let p = normalize_pattern(&project, raw)?;
                    crate::checkout::refuse_other_checkout(
                        tx,
                        &project,
                        &me.address,
                        raw,
                        &p,
                        scope.is_some(),
                    )?;
                    Ok((raw.clone(), p))
                })
                .collect::<Result<Vec<_>>>()?;
            conflicts_for(tx, &project, &me.id, &patterns, exclusive)
        })
    }

    /// Make sure `agent` may edit `paths` (files in `project`) and hold them while it does.
    ///
    /// If another agent holds an overlapping path (in this project or another project of
    /// the same checkout), nothing is taken and the conflicts are returned. Otherwise each
    /// path already under a live exclusive hold of the agent's own is left alone,
    /// and every other path gets a narrow exclusive
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
            let me = crate::identity::resolve_acting_agent(tx, tx.node_id, agent)?;
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
                let (first_notice, wait_recorded, retry_after_seconds) =
                    match attempt(tx, "wait", || {
                        wait_for_conflicts_tx(tx, &me, &project, &conflicts)
                    }) {
                        Ok((first_notice, retry_after)) => (first_notice, true, retry_after),
                        Err(e) => {
                            // The savepoint rolled the wait back, so no ping can follow.
                            // The guard says so (wait_recorded = false) and the hook tells
                            // the agent to check again itself. One hold.wait_failed per
                            // path records which hold ids were announced, so a retry with
                            // the same blockers is a short denial (D62 dedupe) while a new
                            // path or a replacement hold still gets its first notice.
                            // Each path is checked before its own failure event is written.
                            let mut first_notice = false;
                            for (path, blockers) in blocker_sets(&conflicts) {
                                first_notice |= failed_wait_is_new(
                                    tx,
                                    &me.id,
                                    &project.project_id,
                                    path,
                                    &blockers,
                                )?;
                                tx.emit(
                                    &me.address,
                                    "hold.wait_failed",
                                    json!({
                                        "agent_id": me.id, "project_id": project.project_id,
                                        "path": path, "blockers": blockers,
                                        "error": e.to_string(),
                                    }),
                                )?;
                            }
                            (first_notice, false, 30)
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
                    wait_recorded,
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
            let mut covered = Vec::new();
            let mut needed = Vec::new();
            for (raw, pattern) in &patterns {
                match own
                    .iter()
                    .find(|(_, held, _, _)| hold_covers(held, pattern))
                {
                    Some(_) => covered.push(raw.clone()),
                    None => needed.push(pattern.clone()),
                }
            }
            let mut ids = Vec::new();
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
                    tx.emit(&me.address, "hold.wait.finished", json!({ "agent_id": me.id, "project_id": project.project_id, "path": path }))?;
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
                wait_recorded: false,
                retry_after_seconds: 0,
            })
        })
    }

    /// Preflight exact literals with the same target escaping as guard_edit.
    /// Escaping a literal filename may exceed the operator glob cap.
    pub fn check_edit_paths(
        &self,
        agent: &str,
        project: &str,
        paths: &[String],
    ) -> Result<Vec<HoldConflict>> {
        self.write(|tx| {
            let me = crate::identity::resolve_acting_agent(tx, tx.node_id, agent)?;
            let project = find_project(tx, tx.node_id, project)?;
            let patterns = paths
                .iter()
                .map(|raw| {
                    Ok((
                        raw.clone(),
                        globset::escape(&normalize_path(&project, raw)?),
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            conflicts_for(tx, &project, &me.id, &patterns, true)
        })
    }

    /// A presence observation for hooks between tool calls. It never renews a file.
    /// `info` is the hook's client and session id: the beat is also a sign of that session
    /// for the queue's session binding. Returns how many holds were renewed.
    pub fn keepalive(&self, agent: &str, info: &SessionInfo) -> Result<usize> {
        self.write(|tx| {
            let me = crate::identity::resolve_acting_agent(tx, tx.node_id, agent)?;
            touch(tx, &me.id)?;
            crate::queue::session_seen_tx(tx, &me.id, info)?;
            Ok(0)
        })
    }

    /// Renew exactly the named live holds by their stored checkout duration.
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
        if hold_ids.is_empty() {
            return Err(Error::Invalid("renew must name exact hold ids".into()));
        }
        self.write(|tx| {
            sweep_tx(tx)?;
            let me = crate::identity::resolve_acting_agent(tx, tx.node_id, agent)?;
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
            let me = crate::identity::resolve_acting_agent(tx, tx.node_id, agent)?;
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
    /// holds in sibling checkouts of the same repository, for anyone who looks: they are
    /// other work on the repository and block nothing in this checkout (D92).
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
                Some(a) => Some(crate::identity::resolve_acting_agent(tx, tx.node_id, a)?.id),
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

    /// Session heartbeat: touch `last_seen_at`. With `end`,
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
    /// `session.ended` events; a plain `Beat` only updates presence.
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
        if let Some(encoded) = agent.strip_prefix("another-private:") {
            let proof: crate::ActorProof = serde_json::from_slice(&crate::crypto::unb64(encoded)?)
                .map_err(|_| {
                    Error::Forbidden("NO_ACTOR: invalid private session context".into())
                })?;
            if phase == SessionPhase::End {
                let released = self.end_actor_session(
                    &proof,
                    info.session_id.as_deref().unwrap_or("private-session-end"),
                )?;
                let leases = self.right_leases()?;
                let actor = self
                    .contract_actors()?
                    .into_iter()
                    .find(|a| a.actor_id == proof.actor_id)
                    .ok_or_else(|| Error::Forbidden("NO_ACTOR".into()))?;
                return Ok(HeartbeatReport {
                    agent: actor.name,
                    status: "offline".into(),
                    last_seen_at: fmt_ts(self.now()),
                    renewed_holds: vec![],
                    released_holds: leases
                        .iter()
                        .filter(|l| {
                            released.contains(&l.lease_id)
                                && l.subject_kind == crate::SubjectKind::File
                        })
                        .map(|l| l.lease_id.clone())
                        .collect(),
                    resource_leases: leases
                        .iter()
                        .filter(|l| {
                            released.contains(&l.lease_id)
                                && l.subject_kind == crate::SubjectKind::Resource
                        })
                        .count() as u32,
                    unread: 0,
                    urgent_unread: 0,
                    open_requests: 0,
                });
            }
            self.actor_activity(&proof)?;
        }
        self.write(|tx| {
            sweep_tx(tx)?;
            let me = crate::identity::resolve_acting_agent(tx, tx.node_id, agent)?;
            let owned = owned_holds(tx, &me.id, &[], scope)?;
            let session_payload = || {
                json!({
                    "agent_id": me.id,
                    "client": info.client,
                    "session_id": info.session_id,
                })
            };
            let release_leases = phase == SessionPhase::End;
            let resource_leases = if release_leases {
                crate::resources::session_leases_tx(tx, &me, true, scope)?
            } else {
                0
            };
            let (renewed, released, status) = if phase == SessionPhase::End {
                let ids: Vec<String> = owned.into_iter().map(|(id, _)| id).collect();
                release_tx(tx, &me.address, &ids, "session_end")?;
                let ended = tx.emit(&me.address, "session.ended", session_payload())?;
                tx.execute(
                    "UPDATE agents SET status = 'offline', last_seen_at = ?2 WHERE agent_id = ?1",
                    params![me.id, tx.now],
                )?;
                // Only the end hook of the session that made a claim releases it: a remote
                // caller's end, or one that names no session, releases holds and no claim.
                if scope.is_none() {
                    crate::queue::session_ended_tx(tx, &me.id, &info, &ended)?;
                }
                (Vec::new(), ids, "offline")
            } else {
                if phase == SessionPhase::Start {
                    tx.emit(&me.address, "session.started", session_payload())?;
                }
                touch(tx, &me.id)?;
                if scope.is_none() {
                    crate::queue::session_seen_tx(tx, &me.id, &info)?;
                }
                (Vec::new(), Vec::new(), "active")
            };
            // After this agent's own sign of life, and never against its own claims.
            crate::queue::queue_sweep_tx(tx, Some(&me.id))?;
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
    use crate::Program;

    /// #137 adversarial F1: only waits with a blocker that is no longer live are rechecked,
    /// so a write that ends no hold costs nothing per wait; expiry counts before any sweep.
    #[test]
    fn only_waits_with_an_ended_blocker_are_rechecked() {
        let dir = tempfile::tempdir().unwrap();
        let (clock, now) = crate::store::Clock::manual(1_800_000_000_000);
        let store = Store::open_in_memory(clock).unwrap();
        for (name, program) in [("alice", Program::Claude), ("bob", Program::Codex)] {
            store
                .register_agent(crate::RegisterAgent {
                    name: name.into(),
                    program,
                    project: None,
                    model: None,
                    description: None,
                })
                .unwrap();
        }
        let project = store
            .ensure_project("test", dir.path().to_str().unwrap())
            .unwrap()
            .abs_path;
        store
            .acquire_holds(AcquireHolds {
                agent: "alice".into(),
                project: project.clone(),
                paths: vec!["src/**".into()],
                ttl_seconds: Some(600),
                exclusive: true,
                reason: None,
                repo_wide: false,
            })
            .unwrap();
        for path in ["src/a.rs", "src/b.rs"] {
            let blocked = store
                .guard_edit("bob", &project, &[path.into()], "auto")
                .unwrap();
            assert_eq!(blocked.conflicts.len(), 1);
        }
        let candidates = || store.write(waits_to_recheck_tx).unwrap().len();
        assert_eq!(candidates(), 0, "every blocker is live");

        store
            .write(|tx| {
                tx.execute(
                    "INSERT INTO hold_waits (agent_id, project_id, path, blockers, holder_id,
                       retry_at, backoff_seconds)
                     SELECT agent_id, project_id, 'src/c.rs', 'not json', holder_id, retry_at,
                       backoff_seconds FROM hold_waits LIMIT 1",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        assert_eq!(candidates(), 1, "a malformed row is always rechecked");
        store
            .write(|tx| {
                tx.execute("DELETE FROM hold_waits WHERE path = 'src/c.rs'", [])?;
                Ok(())
            })
            .unwrap();

        now.fetch_add(601_000, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            candidates(),
            2,
            "an expired blocker counts before it is swept"
        );
    }

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
            // r7: a chain of alternations past any spelling-out cap still branches one
            // component at a time, in either reading.
            (
                "src/{a/b,c}/{d,e}/{f,g}/{h,i}/{j,k}/{l,m}",
                "src/a/b/d/f/h/j/l/page.tsx",
            ),
            (
                "src/{a/b,c}/{d,e}/{f,g}/{h,i}/{j,k}/{l,m}/[id]/page.tsx",
                "src/a/b/d/f/h/j/l/[id]/page.tsx",
            ),
            (
                "src/{a/b,c}/{d,e}/{f,g}/{h,i}/{j,k}/{l,m}/{n,o}/{p,q}/{r,s}/{t,u}/{v,w}/{x,y}",
                "src/c/e/g/i/k/m/o/q/s/u/w/y/page.tsx",
            ),
            (
                "src/{a/b,c}/{d,e}/{f,g}/{h,i}/{j,k}/{group}",
                "src/a/b/d/f/h/j/{group}/page.tsx",
            ),
            (
                "src/{a/b,c}/{d,e}/{f,g}/{h,i}/{j,k}/**/[slug]/*.tsx",
                "src/c/d/f/h/j/x/y/[slug]/page.tsx",
            ),
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
            // r7: a long chain still names only what it spells.
            (
                "src/{a/b,c}/{d,e}/{f,g}/{h,i}/{j,k}/{l,m}",
                "src/a/b/d/f/h/j/x/page.tsx",
            ),
            (
                "src/{a/b,c}/{d,e}/{f,g}/{h,i}/{j,k}/{l,m}",
                "src/a/d/f/h/j/l/page.tsx",
            ),
            (
                "src/{a/b,c}/{d,e}/{f,g}/{h,i}/{j,k}/{l,m}/[id]/page.tsx",
                "src/a/b/d/f/h/j/l/[idx]/page.tsx",
            ),
            (
                "src/{a/b,c}/{d,e}/{f,g}/{h,i}/{j,k}/{l,m}/[id]/page.tsx",
                "src/a/b/d/f/h/j/l/[id]/page.css",
            ),
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
        assert_eq!(
            first_alternation("{a/b,c}"),
            Some(("", vec!["a/b".to_string(), "c".to_string()], ""))
        );
        assert_eq!(
            first_alternation("{d,}.ts"),
            Some((
                "",
                vec!["d".to_string(), String::new(), "[{]d,[}]".to_string()],
                ".ts"
            ))
        );
        assert_eq!(first_alternation("[{]a,b[}]"), None);
        assert_eq!(first_alternation("src/{a,b"), None);
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
        // r7: a chain of alternations past any spelling-out cap.
        assert!(hold_covers(
            "src/{a/b,c}/{d,e}/{f,g}/{h,i}/{j,k}/{l,m}",
            "src/a/b/d/f/h/j/l/page.tsx"
        ));
        assert!(hold_covers(
            "src/{a/b,c}/{d,e}/{f,g}/{h,i}/{j,k}/{l,m}/[id]/page.tsx",
            "src/a/b/d/f/h/j/l/[id]/page.tsx"
        ));
        assert!(!hold_covers(
            "src/{a/b,c}/{d,e}/{f,g}/{h,i}/{j,k}/{l,m}",
            "src/a/b/d/f/h/j/x/page.tsx"
        ));
    }

    /// r7: the component walk has a step budget instead of a spelling-out cap. A
    /// comparison that would run past it counts as an overlap, never as free, and a
    /// pattern built to make the walk branch at every step still answers quickly.
    #[test]
    fn exhausted_walk_budget_counts_as_overlap() {
        let mut none = 0;
        assert!(components_touch(&["a"], &["b"], false, &mut none));
        let mut plenty = WALK_BUDGET;
        assert!(!components_touch(&["a"], &["b"], false, &mut plenty));
        assert_eq!(plenty, WALK_BUDGET - 1);
        let branching = "{**,x}/".repeat(40) + "end";
        let deep = "a/".repeat(60) + "tail";
        let started = std::time::Instant::now();
        assert!(patterns_overlap(&branching, &globset::escape(&deep)));
        assert!(patterns_overlap(&globset::escape(&deep), &branching));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    /// What honest patterns need, so that the budget can be read against it: globs like
    /// the ones in this file's tests and in the docs, against a 30 component path, a 3,700
    /// byte path and ordinary ones. The most any of them takes is 493 steps (the two
    /// alternations between three `**`); the widest realistic one takes 304. The budget
    /// is at least four times that, and the hostile shape of the Q30 review (an 86 byte
    /// glob of fourteen alternations) is cut off by it.
    #[test]
    fn honest_patterns_stay_far_inside_the_walk_budget() {
        let deep = format!("{}file.test.ts", "packages/web/src/components/".repeat(10));
        let long = format!("src/{}/x.rs", "a-directory-name-of-some-length".repeat(120));
        let globs = [
            "**/*.rs",
            "**/*.test.ts",
            "src/**/*.rs",
            "packages/*/src/**/*.test.ts",
            "packages/**/src/**/components/**/*.test.ts",
            "**/packages/**/web/**/src/**/components/**/*.ts",
            "**/node_modules/**",
            "{src,docs,tests}/**/*.{rs,md,toml}",
            "{packages,apps}/{web,api,mobile}/src/**/*.{ts,tsx,js,jsx}",
            "packages/{web,api}/**/{components,hooks,lib}/**/*.{ts,tsx}",
            "app/[id]/**/page.tsx",
            "app/(group)/[[...slug]]/*.tsx",
            "src/{a/b,c}/{d,e}/{f,g}/{h,i}/{j,k}/{l,m}/[id]/page.tsx",
            "**/{a,b,c,d,e,f,g,h}/**/{i,j,k,l}/**",
            "*/*/*/*/*/*/*/*/*/*",
            "**/*/**/*/**/*.ts",
        ];
        let paths = [
            deep.as_str(),
            long.as_str(),
            "src/a.rs",
            "packages/web/src/components/x/y/z/file.test.ts",
            "README.md",
        ];
        let mut most = 0;
        for glob in globs {
            for path in paths {
                let lit: Vec<&str> = path.split('/').collect();
                let mut budget = usize::MAX;
                components_touch(&pattern_components(glob), &lit, true, &mut budget);
                let used = usize::MAX - budget;
                assert!(used * 4 <= WALK_BUDGET, "{glob} took {used} steps");
                most = most.max(used);
            }
        }
        assert_eq!(most, 493);
        let hostile = format!("{}[!a][!b][!c][!d]", "{a,b}".repeat(14));
        let mut budget = WALK_BUDGET;
        assert_eq!(
            glob_walk(&hostile, "src/a.rs", true, &mut budget),
            Touch::Unknown
        );
        assert!(patterns_overlap(&hostile, "src/a.rs"), "fail closed");
    }

    /// Review R1 of Q30: when the walk runs out, an agent's own costly glob does not count
    /// as covering the edited file, so the edit guard takes a narrow hold. The same glob
    /// still covers what globset itself says it matches.
    #[test]
    fn an_own_glob_too_costly_to_walk_covers_only_what_it_matches_whole() {
        let costly = format!("src/{}[!a][!b][!c][!d]", "{a,b}".repeat(14));
        assert!(!hold_covers(&costly, "src/a.rs"));
        assert!(
            patterns_overlap(&costly, "src/a.rs"),
            "others still conflict"
        );
        assert!(hold_covers(&costly, "src/aaaaaaaaaaaaaabcde"));
    }

    /// Review R2 of Q30: a requested glob is matched directly against another node's hold
    /// on a plain path, with no walk. Wherever the direct match gives an answer it is the
    /// walk's answer, over every pair of these globs and paths; where it could differ it
    /// gives none and the walk is kept.
    #[test]
    fn the_direct_match_agrees_with_the_walk() {
        let globs = [
            "**",
            "**/*.rs",
            "*.md",
            "*",
            "src/*",
            "src/*.rs",
            "src/**",
            "src/**/*.rs",
            "apps/api/src/*.ts",
            "apps/api/**",
            "apps/*/src/*.ts",
            "apps/**/{api,server}/**/*.ts",
            "{apps,packages}/*/src/**/*.{ts,tsx}",
            "{apps,packages}/*/src/*.{ts,tsx}",
            "packages/{web,api}/**/{components,hooks}/**/*.tsx",
            "app/[id]/*.tsx",
            "app/[id]*/**",
            "app/{group}/*",
            "a?c/*",
            "src/a*b/?.rs",
            "*/*/*",
            "src/{a/b,c}/*",
            "apps/{web,api}",
            "docs/**/guide",
            "sr*/**/x*",
        ];
        let paths = [
            "README.md",
            "src",
            "src/a.rs",
            "src/a/b.rs",
            "src/a/b/c/d.rs",
            "src/a*b/x.rs",
            "srcs/a.rs",
            "apps",
            "apps/api",
            "apps/api/src",
            "apps/api/src/server.ts",
            "apps/api/src/deep/server.ts",
            "apps/api/README.md",
            "apps/web/src/features/f1/components/table/Row1.tsx",
            "packages/web/src/hooks/use.tsx",
            "packages/web",
            "app/[id]/page.tsx",
            "app/i/page.tsx",
            "app/{group}/x",
            "app/group/x",
            "abc/x",
            "a/b/c",
            "src/a/b/x",
            "src/c/x",
            "apps/web/x",
            "docs/a/guide",
            "docs/a/guide/b",
            "srx/q/xy",
        ];
        let (mut decided, mut yes) = (0, 0);
        for glob in globs {
            let direct = DirectMatch::of(glob);
            for path in paths {
                let mut budget = usize::MAX;
                let walk = overlap_within(glob, &globset::escape(path), &mut budget);
                assert_ne!(walk, Touch::Unknown);
                if let Some(quick) = direct.as_ref().and_then(|d| d.touches(path)) {
                    assert_eq!(quick, walk == Touch::Yes, "{glob} against {path}");
                    decided += 1;
                    yes += usize::from(quick);
                }
            }
        }
        // Of the 700 pairs, 528 are decided without a walk, 113 of them as overlaps.
        // File-extension alternations stay direct; directory-naming ones keep the walk.
        assert_eq!((decided, yes), (528, 113));
        for direct in [
            "{apps,packages}/*/src/**/*.{ts,tsx}",
            "{apps,packages}/*/src/*.{ts,tsx}",
        ] {
            assert!(DirectMatch::of(direct).is_some(), "{direct}");
        }
        // The shapes that keep the walk, including a last-component alternation that
        // spells a directory name plus a wildcard (review R3 P2-C).
        for kept in [
            "apps/{web,api}",
            "docs/**/guide",
            "src/{a/b,c}/*",
            "src/{lib,*.rs}",
            "packages/{core,*-plugin}",
        ] {
            assert!(DirectMatch::of(kept).is_none(), "{kept}");
        }
        let d = DirectMatch::of("apps/*/src/*.ts").unwrap();
        assert_eq!(d.touches("apps/web"), None, "may be a directory above");
        assert_eq!(
            d.touches("apps/[w]/src/a.ts"),
            None,
            "a bracket in the path"
        );
        assert_eq!(d.touches("packages/web/src/a.ts"), Some(false));
    }

    /// Review R3 of Q30, P2-C: a last-component alternation such as `{core,*-plugin}`
    /// must not be decided by the direct match, which would miss a path under `core`.
    #[test]
    fn a_last_component_alternation_falls_back_to_the_walk() {
        let cases = [
            ("packages/{core,*-plugin}", "packages/core/src/index.ts"),
            ("lib/{util,*-x}", "lib/util/x.rs"),
            ("src/{a,?}", "src/a/b.rs"),
            ("src/{*.rs,a}", "src/a/b.rs"),
            ("src/{lib,*.rs}", "src/lib/mod.rs"),
            ("src/pre{lib,*}.rs", "src/prelib.rs/mod.rs"),
            ("src/{lib,*}{core,?}", "src/libcore/mod.rs"),
        ];
        for (glob, path) in cases {
            assert!(
                DirectMatch::of(glob).is_none(),
                "direct match should fall back for {glob}"
            );
            let mut budget = usize::MAX;
            let walk = overlap_within(glob, &globset::escape(path), &mut budget);
            assert_eq!(walk, Touch::Yes, "{glob} should warn on {path}");
        }
        // `{,app}/*` has an empty alternative, so it also falls back (P3-G).
        assert!(DirectMatch::of("{,app}/*").is_none());
        assert!(DirectMatch::of("packages/core").is_none()); // last component names entries
        assert!(DirectMatch::of("packages/{core,ui}/**").is_some());
        assert!(DirectMatch::of("packages/*/src/*.ts").is_some());
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
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("work/repo");
        let repo_path = repo.to_string_lossy().into_owned();
        let project = Project {
            project_id: "p".into(),
            node_id: "n".into(),
            abs_path: repo_path.clone(),
            slug: "repo".into(),
            created_at: String::new(),
        };
        assert_eq!(normalize_pattern(&project, "./src/").unwrap(), "src");
        assert_eq!(
            normalize_pattern(&project, &repo.join("src/a.rs").to_string_lossy()).unwrap(),
            "src/a.rs"
        );
        assert_eq!(normalize_pattern(&project, &repo_path).unwrap(), "**");
        assert!(normalize_pattern(
            &project,
            &dir.path().join("work/repository/a").to_string_lossy()
        )
        .is_err());
        assert!(normalize_pattern(&project, "../x").is_err());
        assert!(normalize_pattern(&project, "src/[").is_err());
    }
}
