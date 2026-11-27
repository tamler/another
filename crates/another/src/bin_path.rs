//! Locate this process's `another` binary when writing hook wrappers and daemon units.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// The path to record at hook and daemon install: `current_exe` as returned,
/// without following symlinks (a Homebrew Cellar target or rustup shim would
/// break upgrades when the link is updated). On Windows, the `\\?\` verbatim
/// prefix is stripped so the path can be written into schtasks XML.
pub fn resolve_current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the another binary")?;
    Ok(recorded_exe_path(exe))
}

/// Record `exe` as-is, except for a Windows verbatim prefix.
pub fn recorded_exe_path(exe: PathBuf) -> PathBuf {
    PathBuf::from(strip_win32_verbatim(&exe.to_string_lossy()))
}

/// Drop a `\\?\` verbatim prefix. Leaves any other path unchanged, including
/// symlinks. Used so schtasks XML does not receive a `\\?\` path.
pub fn strip_win32_verbatim(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\") {
        if let Some(unc) = rest.strip_prefix(r"UNC\") {
            return format!(r"\\{unc}");
        }
        return rest.to_string();
    }
    path.to_string()
}

/// The `ANOTHER_BIN` path recorded in a hook wrapper script, if present.
pub fn recorded_bin_from_wrapper(script: &str) -> Option<PathBuf> {
    for line in script.lines() {
        let line = line.trim();
        let rest = line.strip_prefix("ANOTHER_BIN=")?;
        let path = unquote_sh(rest);
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    None
}

fn unquote_sh(s: &str) -> String {
    let s = s.trim();
    if let Some(inner) = s.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return inner.replace(r"'\''", "'");
    }
    if let Some(inner) = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return inner.to_string();
    }
    s.to_string()
}

/// Whether `path` names an executable regular file (not followed through a
/// requirement that the path itself exist as a non-symlink).
pub fn is_runnable_exe(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return meta.permissions().mode() & 0o111 != 0;
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_exe_path_keeps_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real-another");
        std::fs::write(&target, b"x").unwrap();
        let link = dir.path().join("shim");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&target, &link).unwrap();
        let recorded = recorded_exe_path(link.clone());
        assert_eq!(
            recorded, link,
            "must not follow the symlink into a Cellar or shim target"
        );
        let meta = std::fs::symlink_metadata(&recorded).unwrap();
        assert!(meta.file_type().is_symlink(), "{recorded:?}");
    }

    #[test]
    fn strip_win32_verbatim_drops_the_prefix_only() {
        assert_eq!(
            strip_win32_verbatim(r"\\?\C:\Program Files\another\another.exe"),
            r"C:\Program Files\another\another.exe"
        );
        assert_eq!(
            strip_win32_verbatim(r"\\?\UNC\server\share\another.exe"),
            r"\\server\share\another.exe"
        );
        assert_eq!(
            strip_win32_verbatim(r"C:\Program Files\another\another.exe"),
            r"C:\Program Files\another\another.exe"
        );
        assert_eq!(
            strip_win32_verbatim("/opt/homebrew/bin/another"),
            "/opt/homebrew/bin/another"
        );
    }

    #[test]
    fn recorded_bin_from_wrapper_reads_the_quoted_path() {
        let script = "#!/bin/sh\nANOTHER_BIN='/opt/another/bin/another'\n";
        assert_eq!(
            recorded_bin_from_wrapper(script).as_deref(),
            Some(Path::new("/opt/another/bin/another"))
        );
    }
}
