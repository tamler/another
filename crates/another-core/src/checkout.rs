//! What follows from holds blocking only inside their checkout (D92), beside the conflict
//! check itself in `holds.rs`:
//!
//! - a hold whose pattern points into another checkout would protect nothing there, so it
//!   is refused with the command that takes it in the right place;
//! - a holder says "I am done with this file" by path. The path is resolved from the file,
//!   the way the edit hook resolves an edit, so it works from any directory and finds the
//!   hold in whichever project of the checkout it was filed under.

use std::path::{Path, PathBuf};

use rusqlite::params;
use serde::Serialize;

use crate::error::{Error, Result};
use crate::holds::{
    folds_case, hold_covers, is_repo_wide, link_unlinked, literal_pattern, literal_prefix,
    normalize_pattern, release_tx, sweep_tx, Hold, HoldFilter,
};
use crate::identity::{find_project, resolve_agent, Project};
use crate::mail::touch;
use crate::repo::{checkout_of, lexical, link_of, resolve_existing};
use crate::store::{Store, Tx};
use crate::time::fmt_ts;
use crate::tokens::{scope_allows_project, TokenScope};

/// A path as a slash-separated string, the way hold patterns spell one.
fn slashed(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// `word` as one word of a shell command: as it is when it holds only characters no shell
/// reads specially, else in single quotes. `=` and `,` are not plain: zsh expands a word
/// that starts with `=`, and a `,` can sit inside a brace expansion.
pub fn shell_word(word: &str) -> String {
    let plain = |c: char| c.is_ascii_alphanumeric() || "%:@._/+-".contains(c);
    if !word.is_empty() && word.chars().all(plain) {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

/// The literal directory part of a normalized pattern and what follows it. A pattern with
/// no wildcard is all directory (or file) and has no rest; for a glob it is the directories
/// before the first wildcard, so `test/app/src/**` gives `test/app/src` and `**`. Empty
/// when the pattern starts with a wildcard.
fn literal_dir(pattern: &str) -> (String, Option<&str>) {
    if let Some(all) = literal_pattern(pattern) {
        return (all, None);
    }
    match literal_prefix(pattern).rfind('/') {
        Some(slash) => (pattern[..slash].to_string(), Some(&pattern[slash + 1..])),
        None => (String::new(), Some(pattern)),
    }
}

/// Refuse a hold (or a check) whose pattern points into another checkout than the one its
/// project belongs to: by D92 it would apply to none of the files it names. The literal
/// directory part of the pattern is resolved physically from the project directory; one
/// that does not exist yet is judged at its nearest existing ancestor. A pattern that
/// starts with a wildcard has no such directory and passes: it covers its own checkout.
///
/// A local caller is given the command that takes the hold where it applies, with
/// `--repo-wide` when the pattern covers that whole checkout, so the command is never one
/// that is refused in turn. A remote caller is given no absolute path: it may name a
/// directory its token does not cover.
pub(crate) fn refuse_other_checkout(
    tx: &Tx<'_>,
    project: &Project,
    agent: &str,
    raw: &str,
    pattern: &str,
    remote: bool,
) -> Result<()> {
    if project.node_id != tx.node_id {
        return Ok(());
    }
    let (dir, rest) = literal_dir(pattern);
    if dir.is_empty() {
        return Ok(());
    }
    let base = resolve_existing(Path::new(&project.abs_path));
    let mut target = resolve_existing(&base.join(&dir));
    // A pattern with no wildcard whose last name is a link to a file (or to nothing). The
    // edit hook holds such a file where its name lives, and also where it points when a
    // hold can exist there.
    if rest.is_none() && !target.is_dir() {
        let written = base.join(&dir);
        if let (Some(parent), Some(name)) = (written.parent(), written.file_name()) {
            let named = resolve_existing(parent).join(name);
            if named != target {
                let around = checkout_of(&named);
                let home = match link_of(tx, project)? {
                    Some(own) => around.as_deref() == Some(Path::new(&own.checkout)),
                    None => around.is_none() && named.starts_with(&base),
                };
                if home {
                    // It points to no checkout and no registered directory: the name is
                    // all the hook holds, so the hold applies where the name lives. A
                    // link to anywhere else is judged at the file it points to, as before.
                    if checkout_of(&target).is_none()
                        && registered_checkout(tx, &local_projects(tx)?, &target)?.is_none()
                    {
                        return Ok(());
                    }
                } else if around.is_some() {
                    // The name itself lives in another checkout, reached through a
                    // directory link: it is judged there, wherever the file link leads.
                    target = named;
                }
            }
        }
    }
    let Some(other) = checkout_of(&target) else {
        // Outside git. Inside the project's own directory, or in a registered directory,
        // it is judged as before. A place a directory link leads to that is in no checkout
        // at all is never held: the hold would apply to nothing.
        if target.starts_with(&base)
            || registered_checkout(tx, &local_projects(tx)?, &target)?.is_some()
        {
            return Ok(());
        }
        let place = if remote {
            String::new()
        } else {
            format!(" ({})", target.to_string_lossy())
        };
        return Err(Error::Invalid(format!(
            "{raw} points outside any checkout{place}; a hold taken here would apply to nothing"
        )));
    };
    match link_of(tx, project)? {
        Some(own) if Path::new(&own.checkout) == other.as_path() => return Ok(()),
        // A project cached as outside git before `git init` ran above it (a kept limit):
        // the checkout found is its own.
        None if base.starts_with(&other) => return Ok(()),
        _ => {}
    }
    if remote {
        return Err(Error::Invalid(format!(
            "{raw} is inside another checkout; a hold taken here would not apply to its \
             files. Take it in that checkout's project."
        )));
    }
    let inside = target.strip_prefix(&other).map(slashed).unwrap_or_default();
    let there = match rest {
        // The caller escaped a literal name: keep it literal in the other checkout too.
        None if dir != pattern => globset::escape(&inside),
        None => inside,
        Some(rest) if inside.is_empty() => rest.to_string(),
        Some(rest) => format!("{inside}/{rest}"),
    };
    let there = if there.is_empty() {
        "**".to_string()
    } else {
        there
    };
    let wide = if is_repo_wide(&there) {
        " --repo-wide"
    } else {
        ""
    };
    let other = other.to_string_lossy();
    Err(Error::Invalid(format!(
        "{raw} is inside another checkout ({other}); a hold taken here would not apply to its \
         files. Take it there: another hold acquire --agent {} --project {}{wide} '{}'",
        shell_word(agent),
        shell_word(&other),
        there.replace('\'', r"'\''"),
    )))
}

/// One argument of a release by path.
#[derive(Debug, Clone)]
pub struct ReleasePath {
    /// The argument as the caller wrote it, for messages.
    pub shown: String,
    /// The same path made absolute (joined to the caller's base directory), not resolved.
    pub path: PathBuf,
    /// The argument may be a mistyped hold id and not a path at all. When no hold of the
    /// caller's names it, covers it or lies below it, that is an error naming both
    /// readings, and nothing is released for any argument, where a plain path would get a
    /// note.
    pub maybe_id: bool,
}

/// A live hold in a listing, with whether it can block in the place the listing was asked
/// from.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ListedHold {
    #[serde(flatten)]
    pub hold: Hold,
    /// The hold was taken in another checkout than the one the listing was asked from, so
    /// it does not block edits there (D92). Always false when the listing was asked from
    /// outside any checkout.
    pub other_checkout: bool,
}

/// What a release by path or by pattern did.
#[derive(Debug, Clone, Default, Serialize, schemars::JsonSchema)]
pub struct PathRelease {
    /// Ids of the holds released.
    pub released: Vec<String>,
    /// One line per argument that released nothing, saying why.
    pub notes: Vec<String>,
}

/// A live hold of the caller's, with what is needed to place it on disk.
struct OwnHold {
    id: String,
    pattern: String,
    project: Project,
    /// `project_repos.checkout` of its project: '' outside git or when not computed.
    checkout: String,
}

fn same_path(a: &Path, b: &Path, fold: bool) -> bool {
    a == b || (fold && a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase())
}

/// `path` is `dir` or lies below it.
fn at_or_below(path: &Path, dir: &Path, fold: bool) -> bool {
    path.starts_with(dir)
        || (fold
            && Path::new(&path.to_string_lossy().to_lowercase())
                .starts_with(dir.to_string_lossy().to_lowercase()))
}

fn local_projects(tx: &Tx<'_>) -> Result<Vec<Project>> {
    let mut stmt = tx.prepare(
        "SELECT project_id, node_id, abs_path, slug, created_at FROM projects
         WHERE node_id = ?1 ORDER BY abs_path",
    )?;
    let projects = stmt
        .query_map([tx.node_id], |r| {
            Ok(Project {
                project_id: r.get(0)?,
                node_id: r.get(1)?,
                abs_path: r.get(2)?,
                slug: r.get(3)?,
                created_at: fmt_ts(r.get(4)?),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(projects)
}

/// The caller's live holds on this node, in projects `scope` may see (all of them locally).
fn own_holds(tx: &Tx<'_>, agent_id: &str, scope: Option<&TokenScope>) -> Result<Vec<OwnHold>> {
    let mut stmt = tx.prepare(
        "SELECT h.hold_id, h.pattern, p.project_id, p.node_id, p.abs_path, p.slug, p.created_at,
           coalesce(pr.checkout, '')
         FROM holds h JOIN projects p ON p.project_id = h.project_id
         LEFT JOIN project_repos pr ON pr.project_id = h.project_id
         WHERE h.agent_id = ?1 AND h.node_id = ?2 AND h.released_at IS NULL
           AND h.expires_at > ?3
         ORDER BY h.created_at, h.hold_id",
    )?;
    let rows = stmt
        .query_map(params![agent_id, tx.node_id, tx.now], |r| {
            Ok(OwnHold {
                id: r.get(0)?,
                pattern: r.get(1)?,
                project: Project {
                    project_id: r.get(2)?,
                    node_id: r.get(3)?,
                    abs_path: r.get(4)?,
                    slug: r.get(5)?,
                    created_at: fmt_ts(r.get(6)?),
                },
                checkout: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut own = Vec::new();
    for hold in rows {
        let visible = match scope {
            Some(s) => scope_allows_project(tx, s, &hold.project.project_id)?,
            None => true,
        };
        if visible {
            own.push(hold);
        }
    }
    Ok(own)
}

/// The checkout of a physically resolved path outside git (D92, rule 2): the outermost
/// registered project directory that contains it and is itself outside git. Whether a
/// project is outside git is taken from its computed link, never from a stored '' alone.
fn registered_checkout(
    tx: &Tx<'_>,
    projects: &[Project],
    target: &Path,
) -> Result<Option<PathBuf>> {
    let mut outermost: Option<PathBuf> = None;
    for project in projects {
        let dir = resolve_existing(Path::new(&project.abs_path));
        if !target.starts_with(&dir) || link_of(tx, project)?.is_some() {
            continue;
        }
        if outermost
            .as_ref()
            .is_none_or(|o| dir.components().count() < o.components().count())
        {
            outermost = Some(dir);
        }
    }
    Ok(outermost)
}

/// The literal directory part of a glob, decoded: its leading components that are plain
/// names, an escaped name included, so `src/br[[]1[]]/**` gives `src/br[1]`. None when the
/// glob starts with a wildcard.
fn glob_dir(pattern: &str) -> Option<String> {
    let dirs: Vec<String> = pattern.split('/').map_while(literal_pattern).collect();
    (!dirs.is_empty()).then(|| dirs.join("/"))
}

impl Store {
    /// [`Store::holds`], with each hold marked when it was taken in another checkout than
    /// the one `here` lies in. A project's checkout is found as for a release by path: in
    /// git from its link, outside git the outermost registered directory around it. With
    /// no `here`, or one in no checkout, nothing is marked.
    pub fn holds_seen_from(
        &self,
        filter: HoldFilter,
        here: Option<&Path>,
    ) -> Result<Vec<ListedHold>> {
        let holds = self.holds(filter)?;
        self.write(|tx| {
            let projects = local_projects(tx)?;
            let here = here.map(resolve_existing);
            let in_git = here.as_deref().and_then(checkout_of);
            let checkout = match (&in_git, &here) {
                (Some(top), _) => Some(top.clone()),
                (None, Some(here)) => registered_checkout(tx, &projects, here)?,
                (None, None) => None,
            };
            let mut listed = Vec::new();
            for hold in holds {
                let project = projects.iter().find(|p| p.slug == hold.project);
                let other_checkout = match (&checkout, project) {
                    (Some(checkout), Some(project)) => match link_of(tx, project)? {
                        Some(link) => {
                            in_git.is_none() || Path::new(&link.checkout) != checkout.as_path()
                        }
                        None => {
                            in_git.is_some()
                                || !at_or_below(
                                    &resolve_existing(Path::new(&project.abs_path)),
                                    checkout,
                                    folds_case(checkout),
                                )
                        }
                    },
                    _ => false,
                };
                listed.push(ListedHold {
                    hold,
                    other_checkout,
                });
            }
            Ok(listed)
        })
    }

    /// Release the holds `agent` has on the given files and directories: "I am done with
    /// this file" (D92). Each path is resolved from the file, as the edit hook does it:
    ///
    /// 1. it is resolved physically ([`resolve_existing`]);
    /// 2. its checkout is found: the nearest directory with a `.git` entry, else the
    ///    outermost registered directory outside git that contains it;
    /// 3. the caller's own live holds in the projects of that checkout that name that
    ///    physical file are released. A hold names a file when its pattern is a literal
    ///    (as every hold of the edit hook is) and, joined to its project's resolved
    ///    directory, is that path; letter case is ignored where the volume ignores it.
    ///
    /// For a directory: the holds whose file is that directory or lies below it, and the
    /// glob holds whose literal directory part does; a glob with no literal directory
    /// stays. A path inside a wider hold of the caller's is not carved out of it, and a
    /// path with nothing of the caller's is not an error: both are reported in `notes`.
    /// No project is registered and nobody else's hold is touched. The event is
    /// `hold.released` with reason `done`.
    ///
    /// For a remote caller (`scope`), each path as written, without following symlinks,
    /// must lie inside the directory of a project its token covers; that is settled before
    /// the disk is read. Only holds in projects the token may see are released, and its
    /// notes name the path as given, never a checkout directory.
    pub fn release_paths(
        &self,
        agent: &str,
        paths: &[ReleasePath],
        scope: Option<&TokenScope>,
    ) -> Result<PathRelease> {
        self.write(|tx| {
            sweep_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let projects = local_projects(tx)?;
            if let Some(scope) = scope {
                for given in paths {
                    let written = lexical(&given.path);
                    let mut covered = false;
                    for project in &projects {
                        if written.starts_with(&project.abs_path)
                            && scope_allows_project(tx, scope, &project.project_id)?
                        {
                            covered = true;
                            break;
                        }
                    }
                    if !covered {
                        return Err(Error::Forbidden(format!(
                            "{} is outside your projects",
                            given.shown
                        )));
                    }
                }
            }
            // Projects with live holds get their checkout computed first, as for conflicts.
            link_unlinked(tx)?;
            let own = own_holds(tx, &me.id, scope)?;
            let mut out = PathRelease::default();
            for given in paths {
                let target = resolve_existing(&given.path);
                let shown = match scope {
                    Some(_) => given.shown.clone(),
                    None => target.to_string_lossy().into_owned(),
                };
                let in_git = checkout_of(&target);
                let checkout = match &in_git {
                    Some(top) => Some(top.clone()),
                    None => registered_checkout(tx, &projects, &target)?,
                };
                let not_an_id = || {
                    Error::NotFound(format!(
                        "no live hold {0}: it is not a hold id (26 characters) and no hold of \
                         yours names the path {shown}; for a file of that name, give ./{0}",
                        given.shown
                    ))
                };
                let Some(checkout) = checkout else {
                    if given.maybe_id {
                        return Err(not_an_id());
                    }
                    out.notes
                        .push(format!("{shown} is in no checkout; nothing is held there"));
                    continue;
                };
                let fold = folds_case(&checkout);
                let is_dir = target.is_dir();
                let mut hit = Vec::new();
                let mut wider: Option<&OwnHold> = None;
                let mut below = false;
                for hold in &own {
                    let dir = resolve_existing(Path::new(&hold.project.abs_path));
                    let member = match &in_git {
                        Some(top) => Path::new(&hold.checkout) == top.as_path(),
                        None => {
                            at_or_below(&dir, &checkout, fold)
                                && link_of(tx, &hold.project)?.is_none()
                        }
                    };
                    if !member {
                        continue;
                    }
                    let named = match literal_pattern(&hold.pattern) {
                        Some(file) => Some(dir.join(file)),
                        // A glob names no file; a directory argument takes it with its
                        // literal directory part.
                        None => glob_dir(&hold.pattern)
                            .filter(|_| is_dir)
                            .map(|inside| dir.join(inside)),
                    };
                    let released = named.as_ref().is_some_and(|at| {
                        same_path(at, &target, fold) || (is_dir && at_or_below(at, &target, fold))
                    });
                    if released {
                        hit.push(hold.id.clone());
                    } else if target
                        .strip_prefix(&dir)
                        .is_ok_and(|rel| hold_covers(&hold.pattern, &slashed(rel)))
                    {
                        wider = Some(hold);
                    } else if glob_dir(&hold.pattern)
                        .is_some_and(|inside| at_or_below(&dir.join(inside), &target, fold))
                    {
                        // A glob of the caller's reaches into a directory of that name
                        // that is not there (yet): the name is no mistyped id.
                        below = true;
                    }
                }
                if !hit.is_empty() {
                    for id in hit {
                        if !out.released.contains(&id) {
                            out.released.push(id);
                        }
                    }
                } else if let Some(hold) = wider {
                    out.notes.push(format!(
                        "{} is inside your hold {} [{}]; release that hold to free it",
                        given.shown, hold.pattern, hold.id
                    ));
                } else if given.maybe_id && !below {
                    return Err(not_an_id());
                } else if scope.is_some() {
                    out.notes
                        .push(format!("nothing of yours is held at {shown}"));
                } else {
                    out.notes.push(format!(
                        "nothing of yours is held at {shown} (checkout {})",
                        checkout.to_string_lossy()
                    ));
                }
            }
            release_tx(tx, &me.address, &out.released, "done")?;
            touch(tx, &me.id)?;
            Ok(out)
        })
    }

    /// Release the hold `agent` has in `project` with exactly the pattern `raw` (normalized
    /// as `hold acquire` normalizes it). Nothing else is matched: a glob names a hold by
    /// its pattern, not the files it covers.
    pub fn release_pattern(&self, agent: &str, project: &str, raw: &str) -> Result<PathRelease> {
        self.write(|tx| {
            sweep_tx(tx)?;
            let me = resolve_agent(tx, tx.node_id, agent)?;
            let project = find_project(tx, tx.node_id, project)?;
            let pattern = normalize_pattern(&project, raw)?;
            let mut stmt = tx.prepare(
                "SELECT hold_id FROM holds WHERE agent_id = ?1 AND project_id = ?2
                   AND pattern = ?3 AND released_at IS NULL AND expires_at > ?4 AND node_id = ?5
                 ORDER BY created_at, hold_id",
            )?;
            let released = stmt
                .query_map(
                    params![me.id, project.project_id, pattern, tx.now, tx.node_id],
                    |r| r.get(0),
                )?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            let mut out = PathRelease {
                released,
                notes: Vec::new(),
            };
            if out.released.is_empty() {
                out.notes.push(format!(
                    "nothing of yours is held as {pattern} in {}",
                    project.slug
                ));
            }
            release_tx(tx, &me.address, &out.released, "released")?;
            touch(tx, &me.id)?;
            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_directory_of_a_pattern() {
        assert_eq!(literal_dir("test/app"), ("test/app".into(), None));
        assert_eq!(
            literal_dir("test/app/src/**"),
            ("test/app/src".into(), Some("**"))
        );
        assert_eq!(
            literal_dir("test/app/sr*"),
            ("test/app".into(), Some("sr*"))
        );
        assert_eq!(literal_dir("src*"), (String::new(), Some("src*")));
        assert_eq!(literal_dir("**/*.rs"), (String::new(), Some("**/*.rs")));
        // An escaped name is a literal, and is decoded.
        assert_eq!(
            literal_dir("app/[[]id[]]/page.tsx"),
            ("app/[id]/page.tsx".into(), None)
        );
    }

    #[test]
    fn literal_directory_of_a_glob_is_decoded() {
        assert_eq!(glob_dir("src/api/**").as_deref(), Some("src/api"));
        assert_eq!(glob_dir("src/a*.rs").as_deref(), Some("src"));
        assert_eq!(glob_dir("src/br[[]1[]]/**").as_deref(), Some("src/br[1]"));
        // A real character class is a wildcard, not a name.
        assert_eq!(glob_dir("src/br[12]/**").as_deref(), Some("src"));
        assert_eq!(glob_dir("**/*.rs"), None);
        assert_eq!(glob_dir("*.txt"), None);
    }

    #[test]
    fn shell_words() {
        assert_eq!(shell_word("/Users/j/app"), "/Users/j/app");
        assert_eq!(shell_word("claude@shop"), "claude@shop");
        assert_eq!(shell_word("/Users/j/my app"), "'/Users/j/my app'");
        assert_eq!(shell_word("it's"), r"'it'\''s'");
        assert_eq!(shell_word("%3"), "%3");
        // Stricter than before: zsh expands `=word`, and `,` may sit in a brace expansion.
        assert_eq!(shell_word("=ls"), "'=ls'");
        assert_eq!(shell_word("a,b"), "'a,b'");
        assert_eq!(shell_word(""), "''");
    }
}
