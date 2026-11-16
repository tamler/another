//! `another hook pre-commit`: the advisory commit guard (docs/HOOKS.md, "Commit guard").
//!
//! It lists the staged files with `git`, checks them read-only against other agents' live
//! holds (`another_core::CommitReader`), and warns on stderr. It never blocks a commit:
//! every outcome, including a missing database, a busy one, a failed or slow `git` and a
//! bad argument, exits 0. Silence means no live overlapping hold was found among the
//! checked paths, never that the files are free.
//!
//! The time budget bounds how long the commit waits, not how long a stuck `git` keeps
//! running: a child that has not finished in time is left alone (never signalled), and the
//! guard stops reading from it.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use another_core::{CommitCheck, CommitGroup, CommitReader};
use serde_json::{json, Value};

use crate::hooks::resolve_existing;

/// The whole budget: both `git` commands and the database read.
pub const BUDGET: Duration = Duration::from_secs(2);
/// Each wait for a database lock.
pub const DB_BUSY: Duration = Duration::from_millis(300);
/// The most stdout read from the staged-file listing.
pub const MAX_LISTING_BYTES: usize = 4 * 1024 * 1024;
/// The most staged paths checked.
pub const MAX_PATHS: usize = 4096;
/// Conflict lines printed before "and N more".
const MAX_LINES: usize = 20;
/// The longest field printed, in characters.
const MAX_FIELD: usize = 200;

/// Pathspec settings that would change what `:/` means.
const PATHSPEC_ENV: [&str; 4] = [
    "GIT_LITERAL_PATHSPECS",
    "GIT_GLOB_PATHSPECS",
    "GIT_NOGLOB_PATHSPECS",
    "GIT_ICASE_PATHSPECS",
];

/// A bounded child run: its stdout and whether a bound cut it, or why it is unusable;
/// and the child handle, kept to reap later.
type RunOutcome = (Result<(Vec<u8>, bool), String>, Option<Child>);

/// What the staged-file listing gave.
#[derive(Debug, PartialEq, Eq)]
pub enum Listing {
    /// Read to EOF, `git` exited 0, under both bounds: the exact staged set.
    Full(Vec<Vec<u8>>),
    /// A bound was reached: the complete entries before it. The total is unknown.
    Cut(Vec<Vec<u8>>),
    /// Not usable; nothing from it is checked.
    Untrusted(String),
}

/// The outcome of one run, rendered by [`render_text`] or [`render_json`].
#[derive(Debug, Default)]
pub struct Outcome {
    /// Why the check could not run at all.
    pub skipped: Option<String>,
    /// The work tree's top level.
    pub top: Option<String>,
    /// Staged paths read (the exact total unless `listing_cut`).
    pub staged: usize,
    pub listing_cut: bool,
    pub check: Option<CommitCheck>,
}

/// Escape one field for a line of terminal output: every control character (C0, DEL,
/// C1) becomes `\xNN` and a backslash becomes `\\`, so no field can start a new line,
/// move the cursor or fake an escape; the field is cut to 200 characters.
pub fn escape_line(s: &str) -> String {
    let mut out = String::new();
    for (n, c) in s.chars().enumerate() {
        if n == MAX_FIELD {
            out.push('…');
            break;
        }
        match c {
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\x{:02X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Run a `git` command in `dir`, reading at most `max_bytes` of stdout, until `deadline`.
/// Returns the bytes, whether a bound was reached first, and the child (to reap later).
/// On a cut the child is not waited for; on a timeout it is neither waited for nor
/// signalled.
fn run_git(
    dir: &Path,
    args: &[&str],
    deadline: Instant,
    max_bytes: usize,
    max_entries: Option<usize>,
) -> RunOutcome {
    run_bounded("git", dir, args, deadline, max_bytes, max_entries)
}

/// [`run_git`] for any program (tests use a shim).
fn run_bounded(
    program: &str,
    dir: &Path,
    args: &[&str],
    deadline: Instant,
    max_bytes: usize,
    max_entries: Option<usize>,
) -> RunOutcome {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for var in PATHSPEC_ENV {
        cmd.env_remove(var);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (Err(format!("git: {e}")), None),
    };
    let Some(mut stdout) = child.stdout.take() else {
        return (Err("git: no output pipe".into()), Some(child));
    };
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 64 * 1024];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    let mut out = Vec::new();
    let mut entries = 0usize;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return (Err("timed out".into()), Some(child));
        }
        match rx.recv_timeout(left) {
            Ok(chunk) => {
                // Counted per chunk: rescanning the whole buffer would be quadratic.
                entries += chunk.iter().filter(|&&b| b == 0).count();
                out.extend_from_slice(&chunk);
                if out.len() > max_bytes || max_entries.is_some_and(|m| entries > m) {
                    // A bound: stop reading. Dropping the receiver ends the reader, so the
                    // child gets a broken pipe; it is not waited for.
                    return (Ok((out, true)), Some(child));
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return (Err("timed out".into()), Some(child));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    // EOF: the exit status decides, within the same deadline.
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return (Ok((out, false)), Some(child)),
            Ok(Some(status)) => return (Err(format!("git exited with {status}")), Some(child)),
            Ok(None) if Instant::now() >= deadline => {
                return (Err("timed out".into()), Some(child));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => return (Err(format!("git: {e}")), Some(child)),
        }
    }
}

/// Split `-z` output into entries. With `cut`, a partial last entry is dropped and at most
/// `MAX_PATHS` entries are kept; without it, the output must end in NUL.
pub fn split_listing(bytes: &[u8], cut: bool) -> Listing {
    if bytes.is_empty() {
        return if cut {
            Listing::Cut(Vec::new())
        } else {
            Listing::Full(Vec::new())
        };
    }
    let complete_end = bytes.iter().rposition(|&b| b == 0);
    if !cut && complete_end != Some(bytes.len() - 1) {
        return Listing::Untrusted("the listing did not end with a NUL".into());
    }
    let body = match complete_end {
        Some(end) => &bytes[..end],
        None => &[][..],
    };
    let mut entries: Vec<Vec<u8>> = if complete_end.is_some() {
        body.split(|&b| b == 0).map(<[u8]>::to_vec).collect()
    } else {
        Vec::new()
    };
    if cut {
        entries.truncate(MAX_PATHS);
        Listing::Cut(entries)
    } else {
        Listing::Full(entries)
    }
}

/// The staged paths of the commit being made in `dir`, with the top level.
fn staged(
    dir: &Path,
    deadline: Instant,
    children: &mut Vec<Child>,
) -> Result<(PathBuf, Listing), String> {
    let (top, child) = run_git(
        dir,
        &["-c", "core.fsmonitor=false", "rev-parse", "--show-toplevel"],
        deadline,
        64 * 1024,
        None,
    );
    children.extend(child);
    let top = match top {
        Ok((bytes, false)) => String::from_utf8(bytes)
            .ok()
            .map(|s| s.trim_end_matches(['\n', '\r']).to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "not a git work tree".to_string())?,
        Ok((_, true)) => return Err("not a git work tree".into()),
        Err(e) if e == "timed out" => return Err("could not list staged files (timed out)".into()),
        Err(e) if e.starts_with("git: ") => {
            return Err(format!("could not list staged files ({e})"))
        }
        Err(_) => return Err("not a git work tree".into()),
    };
    let (out, child) = run_git(
        dir,
        &[
            "-c",
            "core.fsmonitor=false",
            "-c",
            "diff.relative=false",
            "diff",
            "--cached",
            "--name-only",
            "-z",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--ignore-submodules=none",
            "--",
            ":/",
        ],
        deadline,
        MAX_LISTING_BYTES,
        Some(MAX_PATHS),
    );
    children.extend(child);
    let listing = match out {
        Ok((bytes, cut)) => split_listing(&bytes, cut),
        Err(e) => Listing::Untrusted(e),
    };
    Ok((PathBuf::from(top), listing))
}

/// Group staged paths (relative to `top`) by project, the way the edit guard does: the
/// deeper of the git checkout (`top`) and the closest registered project. Only the top
/// level is resolved; the leaf is never followed.
fn group_paths(top: &Path, paths: &[String], registered: &[String]) -> Vec<CommitGroup> {
    let real_top = resolve_existing(top);
    let projects: Vec<(String, PathBuf)> = registered
        .iter()
        .map(|p| (p.clone(), resolve_existing(Path::new(p))))
        .collect();
    let mut groups: Vec<(PathBuf, CommitGroup)> = Vec::new();
    for rel in paths {
        let target = real_top.join(rel);
        let deeper = projects
            .iter()
            .filter(|(_, real)| target.starts_with(real) && target != *real)
            .max_by_key(|(_, real)| real.components().count())
            .filter(|(_, real)| real.components().count() > real_top.components().count())
            .map(|(_, real)| real.clone());
        let root = deeper.unwrap_or_else(|| real_top.clone());
        let Ok(inner) = target.strip_prefix(&root) else {
            continue;
        };
        let inner = inner.to_string_lossy().into_owned();
        let key = projects.iter().find(|(_, real)| *real == root).map_or_else(
            || root.to_string_lossy().into_owned(),
            |(abs, _)| abs.clone(),
        );
        match groups.iter_mut().find(|(r, _)| *r == root) {
            Some((_, g)) => g.paths.push(inner),
            None => groups.push((
                root,
                CommitGroup {
                    dir: key,
                    paths: vec![inner],
                },
            )),
        }
    }
    groups.into_iter().map(|(_, g)| g).collect()
}

fn db_reason(e: &another_core::Error, opening: bool) -> String {
    match e {
        _ if another_core::is_busy_error(e) => "database busy".into(),
        another_core::Error::NotFound(_) => "no database; run `another init`".into(),
        another_core::Error::Invalid(m) if m.starts_with("database schema") => {
            format!("schema mismatch ({m})")
        }
        e if opening => format!("read-only open failed ({e})"),
        e => format!("database read failed ({e})"),
    }
}

/// Run the check for the commit being made in `dir`. Never fails: every problem becomes
/// [`Outcome::skipped`] or an incomplete result.
pub fn check(home: &Path, dir: &Path, agent: Option<&str>) -> Outcome {
    let deadline = Instant::now() + BUDGET;
    let mut children = Vec::new();
    // A bug must not block a commit either: a panic becomes a skipped check. The default
    // panic message is kept off stderr so the one-line contract holds.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        check_inner(home, dir, agent, deadline, &mut children)
    }))
    .unwrap_or_else(|_| Outcome {
        skipped: Some("internal error in the commit guard".into()),
        ..Outcome::default()
    });
    std::panic::set_hook(previous);
    // Reap whatever has already finished; never wait for or signal the rest.
    for mut c in children {
        let _ = c.try_wait();
    }
    outcome
}

fn check_inner(
    home: &Path,
    dir: &Path,
    agent: Option<&str>,
    deadline: Instant,
    children: &mut Vec<Child>,
) -> Outcome {
    let mut out = Outcome::default();
    let (top, listing) = match staged(dir, deadline, children) {
        Ok(v) => v,
        Err(reason) => {
            out.skipped = Some(reason);
            return out;
        }
    };
    out.top = Some(top.to_string_lossy().into_owned());
    let entries = match listing {
        Listing::Full(e) => e,
        Listing::Cut(e) => {
            out.listing_cut = true;
            e
        }
        Listing::Untrusted(reason) => {
            out.skipped = Some(format!("could not list staged files ({reason})"));
            return out;
        }
    };
    out.staged = entries.len();
    if entries.is_empty() {
        return out;
    }
    let mut paths = Vec::new();
    let mut not_utf8 = Vec::new();
    for e in entries {
        match String::from_utf8(e) {
            Ok(p) => paths.push(p),
            Err(e) => not_utf8.push(String::from_utf8_lossy(e.as_bytes()).into_owned()),
        }
    }
    // The open's own reads are two one-row lookups; its lock wait is what could take time,
    // and it gets no more than the budget has left.
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        out.skipped = Some("time budget spent before the database was read".into());
        return out;
    }
    let reader = match CommitReader::open(home, DB_BUSY.min(left)) {
        Ok(r) => r,
        Err(e) => {
            out.skipped = Some(db_reason(&e, true));
            return out;
        }
    };
    let registered = match reader.project_dirs(deadline) {
        Ok(Some(r)) => r,
        Ok(None) => {
            out.skipped = Some("time budget spent before the database was read".into());
            return out;
        }
        Err(e) => {
            out.skipped = Some(db_reason(&e, false));
            return out;
        }
    };
    let groups = group_paths(&top, &paths, &registered);
    match reader.check(agent, &groups, deadline) {
        Ok(mut check) => {
            for p in not_utf8 {
                check.skipped.push(another_core::CommitSkip {
                    path: p,
                    reason: "name is not UTF-8".into(),
                });
            }
            out.check = Some(check);
        }
        Err(e) => out.skipped = Some(db_reason(&e, false)),
    }
    out
}

/// Whether every staged path was listed and checked.
pub fn complete(o: &Outcome) -> bool {
    o.skipped.is_none()
        && !o.listing_cut
        && o.check
            .as_ref()
            .is_none_or(|c| c.skipped.is_empty() && c.links_complete)
}

/// Time left until an RFC 3339 expiry, as `1h10m` or `24m`.
fn left(expires_at: &str) -> String {
    let Some(at) = another_core::parse_ts(expires_at) else {
        return "?".into();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    let mins = ((at - now).max(0) + 59_999) / 60_000;
    if mins >= 60 {
        format!("{}h{:02}m", mins / 60, mins % 60)
    } else {
        format!("{mins}m")
    }
}

/// The stderr text: nothing when no live overlapping hold was found and every path was
/// checked.
pub fn render_text(o: &Outcome) -> String {
    let mut lines = Vec::new();
    if let Some(reason) = &o.skipped {
        lines.push(format!(
            "another: hold check skipped: {}",
            escape_line(reason)
        ));
        return finish(lines);
    }
    let Some(check) = &o.check else {
        // No check ran: nothing was staged, or the listing was cut before its first
        // complete entry. Only the second needs saying.
        if o.listing_cut {
            lines.push(format!(
                "another: checked 0 of at least {} staged file(s); the rest not checked (listing cut at the size limit)",
                o.staged
            ));
        }
        return finish(lines);
    };
    let top = o.top.as_deref().unwrap_or("");
    if check.checked == 0
        && !check.skipped.is_empty()
        && check
            .skipped
            .iter()
            .all(|s| s.reason == "not in a registered project")
    {
        lines.push(format!(
            "another: {} is not a registered project; holds not checked",
            escape_line(top)
        ));
        return finish(lines);
    }
    if !check.conflicts.is_empty() {
        let mut held_paths: Vec<&str> = check.conflicts.iter().map(|c| c.path.as_str()).collect();
        held_paths.dedup();
        lines.push(format!(
            "another: {} staged file(s) are held by another agent (advisory; the commit goes ahead)",
            held_paths.len()
        ));
        if check.agent.is_none() {
            let why = check
                .agent_note
                .as_deref()
                .map(|n| format!(" ({})", escape_line(n)))
                .unwrap_or_default();
            lines.push(format!(
                "  committer unknown{why}: this may include your own holds; set ANOTHER_AGENT or pass --agent"
            ));
        }
        for c in check.conflicts.iter().take(MAX_LINES) {
            let reason = c
                .held
                .reason
                .as_deref()
                .map(|r| format!("\"{}\", ", escape_line(r)))
                .unwrap_or_default();
            lines.push(format!(
                "  {}  held by {} ({}:{}, {}{} left)",
                escape_line(&c.path),
                escape_line(&c.held.agent),
                escape_line(&c.held.project),
                escape_line(&c.held.pattern),
                reason,
                left(&c.held.expires_at)
            ));
        }
        if check.conflicts.len() > MAX_LINES {
            lines.push(format!(
                "  … and {} more",
                check.conflicts.len() - MAX_LINES
            ));
        }
        let first = &check.conflicts[0];
        lines.push(format!(
            "  ask: another send --from <you> --to {} --subject \"Release {}?\" --request",
            escape_line(&first.held.agent),
            escape_line(&first.path)
        ));
    }
    let not_checked = check.skipped.len();
    if not_checked > 0 || o.listing_cut || !check.links_complete {
        let total = if o.listing_cut {
            format!("at least {}", o.staged)
        } else {
            o.staged.to_string()
        };
        let mut reasons: Vec<String> = check.skipped.iter().map(|s| s.reason.clone()).collect();
        if o.listing_cut {
            reasons.push("listing cut at the size limit".into());
        }
        if !check.links_complete {
            reasons.push("time budget".into());
        }
        reasons.sort();
        reasons.dedup();
        lines.push(format!(
            "another: checked {} of {} staged file(s); {} not checked ({})",
            check.checked,
            total,
            not_checked,
            escape_line(&reasons.join("; "))
        ));
    }
    finish(lines)
}

fn finish(lines: Vec<String>) -> String {
    if lines.is_empty() {
        String::new()
    } else {
        lines.join("\n") + "\n"
    }
}

/// The `--json` object.
pub fn render_json(o: &Outcome) -> Value {
    let check = o.check.as_ref();
    let conflicts: Vec<Value> = check.map_or_else(Vec::new, |c| {
        c.conflicts
            .iter()
            .map(|c| {
                json!({
                    "path": c.path,
                    "dir": c.dir,
                    "holder": c.held.agent,
                    "holder_project": c.held.project,
                    "pattern": c.held.pattern,
                    "reason": c.held.reason,
                    "expires_at": c.held.expires_at,
                })
            })
            .collect()
    });
    json!({
        "complete": complete(o),
        "reason": o.skipped,
        "top": o.top,
        "agent": check.and_then(|c| c.agent.clone()),
        "agent_note": check.and_then(|c| c.agent_note.clone()),
        "staged": if o.listing_cut { Value::Null } else { json!(o.staged) },
        "staged_at_least": if o.listing_cut { json!(o.staged) } else { Value::Null },
        "listing_cut": o.listing_cut,
        "checked": check.map_or(0, |c| c.checked),
        "skipped": check.map_or_else(Vec::new, |c| c.skipped.clone()),
        "conflicts": conflicts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_line_neutralizes_every_control_character() {
        assert_eq!(escape_line("src/a.rs"), "src/a.rs");
        assert_eq!(escape_line("a\nanother: forged"), "a\\x0Aanother: forged");
        assert_eq!(escape_line("\r\x1b[2J\x7f"), "\\x0D\\x1B[2J\\x7F");
        assert_eq!(escape_line("c1:\u{85}\u{9b}"), "c1:\\x85\\x9B");
        assert_eq!(escape_line("back\\x0A"), "back\\\\x0A");
        assert_eq!(escape_line("é ü 日本"), "é ü 日本");
        let long = "x".repeat(300);
        let cut = escape_line(&long);
        assert_eq!(cut.chars().count(), MAX_FIELD + 1);
        assert!(cut.ends_with('…'));
    }

    /// The deadline bounds the wait for a child that never finishes, without signalling it.
    #[cfg(unix)]
    #[test]
    fn a_stuck_child_is_abandoned_at_the_deadline() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let shim = dir.path().join("stuck");
        std::fs::write(
            &shim,
            format!(
                "#!/bin/sh\necho $$ > '{}'\nexec /bin/sleep 30\n",
                pid_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = Instant::now();
        let deadline = started + Duration::from_millis(400);
        let (out, child) = run_bounded(
            shim.to_str().unwrap(),
            dir.path(),
            &[],
            deadline,
            1024,
            None,
        );
        let took = started.elapsed();
        assert_eq!(out, Err("timed out".to_string()));
        assert!(took < Duration::from_millis(700), "{took:?}");
        let mut child = child.expect("the child handle is kept");
        assert!(
            child.try_wait().unwrap().is_none(),
            "the child was stopped; the guard must leave it alone"
        );
        // The test started this child; stop it by its own handle.
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn listings_are_trusted_only_when_complete() {
        assert_eq!(
            split_listing(b"a\0b c\0", false),
            Listing::Full(vec![b"a".to_vec(), b"b c".to_vec()])
        );
        assert_eq!(split_listing(b"", false), Listing::Full(vec![]));
        assert!(matches!(
            split_listing(b"a\0b", false),
            Listing::Untrusted(_)
        ));
        // A cut keeps complete entries and drops the partial one.
        assert_eq!(
            split_listing(b"a\0b\0par", true),
            Listing::Cut(vec![b"a".to_vec(), b"b".to_vec()])
        );
        assert_eq!(split_listing(b"partial", true), Listing::Cut(vec![]));
        let many: Vec<u8> = (0..MAX_PATHS + 5)
            .flat_map(|i| format!("f{i}\0").into_bytes())
            .collect();
        let Listing::Cut(entries) = split_listing(&many, true) else {
            panic!("expected a cut")
        };
        assert_eq!(entries.len(), MAX_PATHS);
    }
}
