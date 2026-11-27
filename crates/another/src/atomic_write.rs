//! Atomic file replacement using unique sibling temp files and stale temp cleanup.
//!
//! Temp names are unique per write (`<target>.another-tmp.<pid>.<seq>.<rand>`) so writes do
//! not reuse a fixed path, and stale sibling temp files left behind by a crash are swept on
//! write and uninstall. Concurrent installs are not supported.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};

use anyhow::{bail, Result};

const TEMP_MARKER: &str = ".another-tmp.";

static COUNTER: AtomicU64 = AtomicU64::new(0);
static IN_FLIGHT: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

fn in_flight() -> MutexGuard<'static, HashSet<PathBuf>> {
    IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner())
}

fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

struct TempGuard {
    /// Set until `rename` succeeds; dropped on any error or panic to remove the temp file.
    unlink: Option<PathBuf>,
    /// Removed from `IN_FLIGHT` on drop so same-process writers do not sweep active temps.
    tracked: PathBuf,
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if let Some(tmp) = self.unlink.take() {
            let _ = std::fs::remove_file(tmp);
        }
        in_flight().remove(&self.tracked);
    }
}

/// Atomically replace `path` with `bytes`: create a unique sibling temp file, write and
/// `sync_all` it, close the handle, run `prepare_tmp` on the temp path, rename it over
/// `path`, and sweep stale sibling temp files matching `<target file name>.another-tmp.*`.
///
/// Temp names are unique per write; concurrent installs are not supported.
pub(crate) fn write_file<F>(
    path: &Path,
    bytes: &[u8],
    create_mode: Option<u32>,
    prepare_tmp: F,
) -> Result<()>
where
    F: FnOnce(&Path) -> Result<()>,
{
    let Some(target_name) = path.file_name() else {
        bail!("target path has no file name: {}", path.display());
    };
    let parent = parent_dir(path);

    let (mut guard, file) = {
        let mut attempt = 0u8;
        loop {
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let rand = another_core::crypto::random_hex(4)?;
            let mut name = target_name.to_os_string();
            name.push(format!("{TEMP_MARKER}{}.{seq}.{rand}", std::process::id()));
            let tmp = parent.join(&name);

            if !in_flight().insert(tmp.clone()) {
                if attempt < 15 {
                    attempt += 1;
                    continue;
                }
                bail!("could not allocate unique temp path for {}", path.display());
            }

            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            if let Some(mode) = create_mode {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(mode);
            }
            #[cfg(not(unix))]
            let _ = create_mode;

            match opts.open(&tmp) {
                Ok(file) => {
                    let guard = TempGuard {
                        unlink: Some(tmp.clone()),
                        tracked: tmp,
                    };
                    break (guard, file);
                }
                Err(err) => {
                    in_flight().remove(&tmp);
                    if err.kind() == std::io::ErrorKind::AlreadyExists && attempt < 15 {
                        attempt += 1;
                        continue;
                    }
                    return Err(err.into());
                }
            }
        }
    };

    // Write and sync inside an inner scope so the handle is always closed before
    // `prepare_tmp`, `rename`, or `TempGuard::drop` removes the file on error.
    {
        let mut file = file;
        file.write_all(bytes)?;
        file.sync_all()?;
    }

    let tmp = guard.tracked.clone();
    prepare_tmp(&tmp)?;
    std::fs::rename(&tmp, path)?;
    guard.unlink = None;
    drop(guard);

    sweep_stale_temps(path);
    Ok(())
}

/// Remove stale sibling temp files matching `<target file name>.another-tmp.*` in the same
/// directory as `target`.
///
/// Only regular files are removed: symlinks and directories are skipped without following
/// them. Missing directories and files that vanish concurrently are ignored.
pub(crate) fn sweep_stale_temps(target: &Path) {
    let Some(target_name) = target.file_name() else {
        return;
    };
    let parent = parent_dir(target);
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };

    let mut prefix = target_name.to_os_string();
    prefix.push(TEMP_MARKER);
    let prefix_bytes = prefix.as_encoded_bytes();

    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let name = entry.file_name();
        let name_bytes = name.as_encoded_bytes();
        if !name_bytes.starts_with(prefix_bytes) || name_bytes.len() == prefix_bytes.len() {
            continue;
        }
        let path = entry.path();
        if in_flight().contains(&path) {
            continue;
        }
        if std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.file_type().is_file()) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_ignores_missing_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist").join("settings.json");
        sweep_stale_temps(&missing);
    }

    #[test]
    fn sweep_leaves_empty_suffix_and_non_matching_names_alone() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("settings.json");
        let stale = dir.path().join("settings.json.another-tmp.123.0.abcd");
        let empty_suffix = dir.path().join("settings.json.another-tmp.");
        let no_dot = dir.path().join("settings.json.another-tmp");
        let other = dir.path().join("other.json.another-tmp.123.0.abcd");
        for p in [&stale, &empty_suffix, &no_dot, &other] {
            std::fs::write(p, "data").unwrap();
        }

        sweep_stale_temps(&target);

        assert!(!stale.exists());
        assert!(empty_suffix.exists());
        assert!(no_dot.exists());
        assert!(other.exists());
    }
}
