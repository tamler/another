//! Where a project sits in git: the repository it belongs to (the same for every worktree
//! and clone, used by the board to list sibling checkouts) and its checkout, the working
//! directory holds block inside (D92).
//!
//! Everything is read from files (`.git`, `commondir`, `config`); no `git` process runs.
//! The key is the `origin` remote reduced to `host/path` (no scheme, credentials, port or
//! `.git` suffix), or the repository's shared git directory when there is no origin.

use std::path::{Component, Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::Result;
use crate::identity::Project;
use crate::store::Tx;

/// Where a project directory sits inside a git repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoLink {
    /// The same for every worktree and clone of one repository.
    pub key: String,
    /// The project directory relative to the repository's top level ("" at the top).
    pub prefix: String,
    /// The working directory the project belongs to: the nearest directory at or above it
    /// with a `.git` entry, as the filesystem resolves it ([`resolve_existing`]). A linked
    /// worktree, a clone and a submodule each have their own; holds block only inside it.
    pub checkout: String,
}

/// Symlinks followed while resolving one path before giving up (a loop).
const MAX_SYMLINKS: usize = 40;

/// `path` as the filesystem resolves it, the way `open` follows it:
///
/// - components are taken left to right and each symlink is followed before any later
///   `..` is applied, so `link/../a` is "the parent of where `link` points, then `a`",
///   not the lexical `a`;
/// - a dangling symlink is read and followed to its (not yet existing) target;
/// - once a component does not exist, the rest is applied lexically, since a missing
///   directory cannot be a link;
/// - the existing part is then canonicalized, which also restores on-disk letter case.
///
/// On a symlink loop (or a chain longer than the OS allows), `path` is used as given with
/// its longest resolvable leading part canonicalized (it stops at the loop, even before a
/// later `..`), so it still names the registered project.
/// `path` should be absolute.
pub fn resolve_existing(path: &Path) -> PathBuf {
    let mut budget = MAX_SYMLINKS;
    canonical_prefix(&follow_links(path, &mut budget).unwrap_or_else(|| path.to_path_buf()))
}

fn follow_links(path: &Path, budget: &mut usize) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    let mut missing = false;
    for comp in path.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => out.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                // `out` is fully resolved (or a missing, so link-free, tail): its parent is
                // the physical parent. Climbing out of a missing tail can land on an
                // existing directory again, whose entries may be links.
                out.pop();
                missing = std::fs::symlink_metadata(&out).is_err();
            }
            Component::Normal(name) => {
                let next = out.join(name);
                if missing {
                    out = next;
                    continue;
                }
                match std::fs::symlink_metadata(&next) {
                    Err(_) => {
                        missing = true;
                        out = next;
                    }
                    Ok(meta) if meta.file_type().is_symlink() => {
                        *budget = budget.checked_sub(1)?;
                        let target = std::fs::read_link(&next).ok()?;
                        out = follow_links(&out.join(target), budget)?;
                        missing = std::fs::symlink_metadata(&out).is_err();
                    }
                    Ok(_) => out = next,
                }
            }
        }
    }
    Some(out)
}

/// Canonicalize the longest leading run of `path`'s components that resolves, and keep the
/// rest as written. For a link-free path that is its deepest existing ancestor; for the
/// fallback path of a symlink loop, which may still hold `..`, it stops at the loop.
fn canonical_prefix(path: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    let comps: Vec<Component> = path.components().collect();
    let mut cur = PathBuf::new();
    let mut real = None;
    let mut kept = 0;
    for (i, comp) in comps.iter().enumerate() {
        cur.push(comp.as_os_str());
        match std::fs::canonicalize(&cur) {
            Ok(r) => {
                real = Some(r);
                kept = i + 1;
            }
            Err(_) => break,
        }
    }
    let Some(mut out) = real else {
        return path.to_path_buf();
    };
    for comp in &comps[kept..] {
        out.push(comp.as_os_str());
    }
    out
}

/// The checkout of a physically resolved path (`resolve_existing`): the nearest directory
/// at or above it that has a `.git` entry, a directory or the file a linked worktree or a
/// submodule has. None when the path is not inside git.
pub(crate) fn checkout_of(resolved: &Path) -> Option<PathBuf> {
    resolved
        .ancestors()
        .find(|d| d.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Git metadata files we read are small; anything bigger is not what we expect.
const MAX_GIT_FILE: u64 = 256 * 1024;

fn read_small(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_GIT_FILE {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// Resolve `rel` against `base` lexically (no filesystem access).
fn join_lexical(base: &Path, rel: &str) -> PathBuf {
    let rel = Path::new(rel);
    if rel.is_absolute() {
        lexical(rel)
    } else {
        lexical(&base.join(rel))
    }
}

/// `path` with `.` and `..` applied as text (no filesystem access, no symlinks followed).
pub(crate) fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The repository a directory belongs to, if any.
pub fn link_for(dir: &Path) -> Option<RepoLink> {
    let top = dir.ancestors().find(|d| d.join(".git").exists())?;
    let prefix = dir
        .strip_prefix(top)
        .ok()?
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    let dot_git = top.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        // A linked worktree or submodule: `.git` is a file naming the real git dir.
        let text = read_small(&dot_git)?;
        let target = text
            .lines()
            .find_map(|l| l.trim().strip_prefix("gitdir:"))?;
        // Git writes a relative gitdir from the `.git` file's real location, so resolve
        // symlinks in `top` first: a worktree opened through a symlink still finds its git dir.
        let real_top = std::fs::canonicalize(top).unwrap_or_else(|_| top.to_path_buf());
        join_lexical(&real_top, target.trim())
    };
    let common = match read_small(&git_dir.join("commondir")) {
        Some(c) if !c.trim().is_empty() => join_lexical(&git_dir, c.trim()),
        _ => git_dir,
    };
    let key = read_small(&common.join("config"))
        .and_then(|c| origin_url(&c))
        .and_then(|u| normalize_remote(&u))
        .unwrap_or_else(|| {
            let canonical = std::fs::canonicalize(&common).unwrap_or(common);
            format!("git:{}", canonical.to_string_lossy())
        });
    // Resolved physically, so one checkout registered under two spellings (a symlinked
    // parent, `/tmp` and `/private/tmp`) has one value.
    let checkout = resolve_existing(top).to_string_lossy().into_owned();
    Some(RepoLink {
        key,
        prefix,
        checkout,
    })
}

/// `url` of `[remote "origin"]` in a git config file.
pub(crate) fn origin_url(config: &str) -> Option<String> {
    let mut in_origin = false;
    for line in config.lines() {
        let line = line.trim();
        if let Some(header) = line.strip_prefix('[') {
            let header = header.split(']').next().unwrap_or_default().trim();
            in_origin = match header.split_once(char::is_whitespace) {
                Some((section, sub)) => {
                    section.eq_ignore_ascii_case("remote")
                        && sub.trim().trim_matches('"') == "origin"
                }
                // Old `[remote.origin]` form.
                None => header.eq_ignore_ascii_case("remote.origin"),
            };
            continue;
        }
        if !in_origin {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            if key.trim().eq_ignore_ascii_case("url") {
                let value = value.trim();
                let value = match value.strip_prefix('"') {
                    Some(rest) => rest.split('"').next().unwrap_or_default(),
                    None => value
                        .split([' ', '\t'])
                        .find(|s| !s.is_empty())
                        .unwrap_or_default(),
                };
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

fn clean_repo_path(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let mut joined = parts.join("/");
    if joined.to_ascii_lowercase().ends_with(".git") {
        joined.truncate(joined.len() - 4);
    }
    joined.trim_end_matches('/').to_string()
}

fn strip_port(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        // [ipv6]:port
        return rest.split(']').next().unwrap_or(rest);
    }
    host.split(':').next().unwrap_or(host)
}

/// `host/path` with the host lowercased (hostnames are case-insensitive) and the path kept
/// as written: a Git server may treat `Team/Shop` and `team/shop` as two repositories.
fn remote_key(host: &str, path: &str) -> String {
    format!("{}/{}", host.to_ascii_lowercase(), clean_repo_path(path))
}

/// Reduce a git remote URL to `host/path` so every spelling of one repository matches.
/// Credentials are always dropped; they never reach the database.
pub fn normalize_remote(url: &str) -> Option<String> {
    let url = url.trim().trim_matches('"').trim();
    if url.is_empty() {
        return None;
    }
    let local = |p: &str| Some(format!("file:/{}", clean_repo_path(p)));
    if let Some((scheme, rest)) = url.split_once("://") {
        if scheme.eq_ignore_ascii_case("file") {
            return local(rest);
        }
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let host = strip_port(authority.rsplit('@').next().unwrap_or(authority));
        if host.is_empty() {
            return None;
        }
        return Some(remote_key(host, path));
    }
    // scp-like `user@host:path`: only when no slash comes before the first colon (git's rule);
    // a single letter before the colon is a Windows drive.
    match url.find(':') {
        Some(colon) if colon > 1 && !url[..colon].contains('/') => {
            let left = &url[..colon];
            let host = left.rsplit('@').next().unwrap_or(left);
            if host.is_empty() {
                return None;
            }
            Some(remote_key(host, &url[colon + 1..]))
        }
        _ => local(url),
    }
}

/// Version of the key rules (`normalize_remote`, `link_for`), stored in front of every
/// cached key. Bump it whenever those rules change: cached rows from an older version are
/// then treated as never checked and recomputed on next use, so a stale row can never
/// hide a hold in another project of the checkout. This needs no schema change, so it never blocks an installed
/// binary from opening the database.
///
/// v3: the link also carries the project's checkout (D92).
/// v4: relative gitdir resolved from the `.git` file's real location.
pub(crate) const KEY_VERSION: &str = "v4:";

/// A key as stored in `project_repos` (empty stays empty: "not in a repository").
pub(crate) fn stored_key(key: &str) -> String {
    if key.is_empty() {
        String::new()
    } else {
        format!("{KEY_VERSION}{key}")
    }
}

/// The cached link: `None` when never checked (or cached under older key rules, or in a
/// repository with no checkout stored yet), `Some(None)` when checked and not in a repo.
///
/// A key beside an empty checkout is a row from before the checkout column that somehow
/// already carries the current key version. It counts as never checked, so filling the
/// column never depends on the version having been raised.
fn cached(conn: &Connection, project_id: &str) -> Result<Option<Option<RepoLink>>> {
    let row: Option<(String, String, String)> = conn
        .query_row(
            "SELECT repo_key, repo_prefix, checkout FROM project_repos WHERE project_id = ?1",
            [project_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    Ok(match row {
        None => None,
        Some((key, _, _)) if key.is_empty() => Some(None),
        Some((_, _, checkout)) if checkout.is_empty() => None,
        Some((key, prefix, checkout)) => key.strip_prefix(KEY_VERSION).map(|k| {
            Some(RepoLink {
                key: k.to_string(),
                prefix,
                checkout,
            })
        }),
    })
}

/// Recompute a local project's repository link from its files and cache it (writing only
/// when it changed). Projects of other nodes have no link: their paths are not on this disk.
pub(crate) fn refresh_link(tx: &Tx<'_>, project: &Project) -> Result<Option<RepoLink>> {
    if project.node_id != tx.node_id {
        return Ok(None);
    }
    let fresh = link_for(Path::new(&project.abs_path));
    if cached(tx, &project.project_id)?.as_ref() != Some(&fresh) {
        let (key, prefix, checkout) = fresh.as_ref().map_or((String::new(), "", ""), |l| {
            (stored_key(&l.key), l.prefix.as_str(), l.checkout.as_str())
        });
        tx.execute(
            "INSERT INTO project_repos (project_id, repo_key, repo_prefix, checkout, checked_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(project_id) DO UPDATE SET repo_key = excluded.repo_key,
               repo_prefix = excluded.repo_prefix, checkout = excluded.checkout,
               checked_at = excluded.checked_at",
            params![project.project_id, key, prefix, checkout, tx.now],
        )?;
    }
    Ok(fresh)
}

/// A project's repository link, from the cache or (first time) from its files.
pub(crate) fn link_of(tx: &Tx<'_>, project: &Project) -> Result<Option<RepoLink>> {
    if project.node_id != tx.node_id {
        return Ok(None);
    }
    match cached(tx, &project.project_id)? {
        Some(link) => Ok(link),
        None => refresh_link(tx, project),
    }
}

/// `pattern` (relative to a project at `prefix` inside its repository) relative to the
/// repository's top level.
pub(crate) fn in_repo(prefix: &str, pattern: &str) -> String {
    if prefix.is_empty() {
        pattern.to_string()
    } else {
        format!("{prefix}/{pattern}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes_normalize_to_one_key() {
        let want = Some("github.com/tamler/another".to_string());
        for url in [
            "git@github.com:tamler/another.git",
            "https://github.com/tamler/another",
            "https://GitHub.com/tamler/another.git/",
            "ssh://git@github.com:22/tamler/another.git",
            "git+ssh://git@github.com/tamler/another",
            "github.com:tamler/another",
        ] {
            assert_eq!(normalize_remote(url), want, "{url}");
        }
        // Credentials never survive.
        let with_token =
            normalize_remote("https://x-access-token:SECRET@github.com/tamler/another.git");
        assert_eq!(with_token, want);
        // Review F4: only the host is case-insensitive. Path case is kept, so two repos
        // whose paths differ only in case on a case-sensitive server stay distinct.
        assert_eq!(
            normalize_remote("git@Example.invalid:Team/Shop.git").as_deref(),
            Some("example.invalid/Team/Shop")
        );
        assert_ne!(
            normalize_remote("git@example.invalid:Team/Shop.git"),
            normalize_remote("https://example.invalid/team/shop")
        );
        assert_eq!(
            normalize_remote("/srv/git/shop.git"),
            Some("file:/srv/git/shop".into())
        );
        assert_eq!(
            normalize_remote("file:///srv/git/shop.git"),
            Some("file:/srv/git/shop".into())
        );
        assert_eq!(normalize_remote("  "), None);
    }

    #[test]
    fn reads_origin_from_config() {
        let config = "[core]\n\tbare = false\n[remote \"upstream\"]\n\turl = https://example.com/up/x.git\n\
                      [remote \"origin\"]\n\turl = git@example.com:me/x.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n";
        assert_eq!(
            origin_url(config).as_deref(),
            Some("git@example.com:me/x.git")
        );
        assert_eq!(origin_url("[remote \"upstream\"]\nurl = a\n"), None);
        assert_eq!(
            origin_url("[remote.origin]\nurl = \"https://h/x\" # note\n").as_deref(),
            Some("https://h/x")
        );
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn worktrees_share_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        write(
            &main.join(".git/config"),
            "[remote \"origin\"]\n\turl = https://user:pw@example.com/me/shop.git\n",
        );
        let wt = dir.path().join("wt-a");
        let wt_git = main.join(".git/worktrees/wt-a");
        write(&wt.join(".git"), &format!("gitdir: {}\n", wt_git.display()));
        write(&wt_git.join("commondir"), "../..\n");
        std::fs::create_dir_all(main.join("src")).unwrap();

        let a = link_for(&main).unwrap();
        let b = link_for(&wt).unwrap();
        assert_eq!(a.key, "example.com/me/shop");
        assert_eq!(a.key, b.key);
        assert_eq!(b.prefix, "");
        // One repository, two checkouts; a subdirectory belongs to the checkout above it.
        assert_ne!(a.checkout, b.checkout);
        assert_eq!(Path::new(&a.checkout), resolve_existing(&main));
        assert_eq!(Path::new(&b.checkout), resolve_existing(&wt));
        let sub = link_for(&main.join("src")).unwrap();
        assert_eq!(sub.prefix, "src");
        assert_eq!(sub.checkout, a.checkout);

        // No origin: the shared git dir is the key, still the same for both checkouts.
        write(&main.join(".git/config"), "[core]\n");
        let a = link_for(&main).unwrap();
        let b = link_for(&wt).unwrap();
        assert!(a.key.starts_with("git:"), "{}", a.key);
        assert_eq!(a.key, b.key);

        let other = dir.path().join("other");
        std::fs::create_dir_all(other.join(".git")).unwrap();
        assert_ne!(link_for(&other).unwrap().key, a.key);
        assert_eq!(link_for(Path::new("/nonexistent/placeholder/app")), None);
    }
}
