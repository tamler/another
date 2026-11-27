//! Atomic file replacement using unique sibling temp files and stale temp cleanup.
//!
//! Temp names are unique per write and bounded to fit comfortably within `NAME_MAX` (255
//! bytes) even when the target file name is near the filesystem limit. The three branches
//! produce pairwise disjoint prefixes by construction:
//! - Short target names (`<= 80` bytes): `<target>.another-tmp.<pid>.<seq>.<rand>`
//! - Longer UTF-8 target names: `<truncated>.another-tmp-long.<hash16>.<pid>.<seq>.<rand>`
//!   (truncated on a UTF-8 char boundary, disambiguated by 16 hex chars of SHA-256 of the
//!   full target name)
//! - Longer non-UTF-8 target names: `.another-tmp-raw.<hash16>.<pid>.<seq>.<rand>`
//!
//! Stale sibling temp files left behind by a crash are swept on write and uninstall only when
//! the filename matches the target's exact prefix AND the writer's exact `<pid>.<seq>.<rand>`
//! suffix grammar (canonical decimal `u32`, canonical decimal `u64`, 8 lowercase hex digits).
//! Concurrent installs are not supported.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};

use anyhow::{bail, Result};

const TEMP_MARKER_SHORT: &str = ".another-tmp.";
const TEMP_MARKER_LONG_UTF8: &str = ".another-tmp-long.";
const TEMP_MARKER_LONG_RAW: &str = ".another-tmp-raw.";
/// Maximum target file name byte length kept verbatim before `.another-tmp.<pid>.<seq>.<rand>`.
///
/// The unique suffix `.another-tmp.<pid>.<seq>.<rand>` is at most 53 ASCII bytes
/// (`13 + 10 + 1 + 20 + 1 + 8`), so `80 + 53 = 133 <= 255` (`NAME_MAX` on Linux/macOS/NTFS
/// and also below eCryptfs's 143-byte limit).
const MAX_INLINE_NAME_BYTES: usize = 80;
/// Maximum bytes kept from a long UTF-8 target name before `.another-tmp-long.<hash16>.`.
///
/// `60 + 18 + 16 + 1 + 40 = 135 <= 255` (and `<= 143` for eCryptfs).
const MAX_TRUNCATED_NAME_BYTES: usize = 60;
/// Hex characters taken from the SHA-256 of the full target file name when shortening.
const HASH_HEX_LEN: usize = 16;
/// Hex characters produced by `another_core::crypto::random_hex(4)`.
const RAND_HEX_LEN: usize = 8;

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

fn floor_char_boundary(s: &str, max_bytes: usize) -> usize {
    let mut end = max_bytes.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn is_canonical_u32(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| b.is_ascii_digit())
        && (s.len() == 1 || !s.starts_with('0'))
        && s.parse::<u32>().is_ok()
}

fn is_canonical_u64(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| b.is_ascii_digit())
        && (s.len() == 1 || !s.starts_with('0'))
        && s.parse::<u64>().is_ok()
}

/// True iff `suffix` matches the exact `<pid>.<seq>.<rand>` grammar produced by [`write_file`]:
/// canonical decimal `u32`, canonical decimal `u64`, and 8 lowercase ASCII hex digits.
fn is_valid_temp_suffix(suffix: &[u8]) -> bool {
    let Ok(s) = std::str::from_utf8(suffix) else {
        return false;
    };
    let mut parts = s.split('.');
    let (Some(pid), Some(seq), Some(rand), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    is_canonical_u32(pid)
        && is_canonical_u64(seq)
        && rand.len() == RAND_HEX_LEN
        && rand.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Deterministic same-directory temp file prefix for `target_name`, bounded so that
/// `<prefix><pid>.<seq>.<rand>` always fits within `NAME_MAX`.
///
/// The short, long UTF-8, and long non-UTF-8 branches are pairwise disjoint:
/// - Short ends with `.another-tmp.` (final segment is `another-tmp`, never 16 hex chars).
/// - Long UTF-8 ends with `.another-tmp-long.<hash16>.`.
/// - Long non-UTF-8 is `.another-tmp-raw.<hash16>.`.
pub(crate) fn temp_prefix(target_name: &OsStr) -> OsString {
    let encoded = target_name.as_encoded_bytes();
    if encoded.len() <= MAX_INLINE_NAME_BYTES {
        let mut prefix = target_name.to_os_string();
        prefix.push(TEMP_MARKER_SHORT);
        return prefix;
    }

    let digest = another_core::crypto::sha256_hex(encoded);
    let hash = &digest[..HASH_HEX_LEN];
    if let Some(utf8) = target_name.to_str() {
        let base = &utf8[..floor_char_boundary(utf8, MAX_TRUNCATED_NAME_BYTES)];
        OsString::from(format!("{base}{TEMP_MARKER_LONG_UTF8}{hash}."))
    } else {
        OsString::from(format!("{TEMP_MARKER_LONG_RAW}{hash}."))
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
/// `path`, and sweep stale sibling temp files matching [`temp_prefix`] for `path`.
///
/// Temp names are unique per write and bounded within `NAME_MAX`; concurrent installs are not
/// supported.
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
    let prefix = temp_prefix(target_name);

    let (mut guard, file) = {
        let mut attempt = 0u8;
        loop {
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let rand = another_core::crypto::random_hex(4)?;
            let mut name = prefix.clone();
            name.push(format!("{}.{seq}.{rand}", std::process::id()));
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

/// Remove stale sibling temp files matching [`temp_prefix`] for `target` in the same
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

    let prefix = temp_prefix(target_name);
    let prefix_bytes = prefix.as_encoded_bytes();

    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let name = entry.file_name();
        let name_bytes = name.as_encoded_bytes();
        let Some(suffix) = name_bytes.strip_prefix(prefix_bytes) else {
            continue;
        };
        if !is_valid_temp_suffix(suffix) {
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

    /// Worst-case length of `<pid>.<seq>.<rand>`: 10 + 1 + 20 + 1 + 8 = 40 ASCII bytes.
    const MAX_UNIQUE_SUFFIX_BYTES: usize = 40;

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
        let stale = dir.path().join("settings.json.another-tmp.123.0.deadbeef");
        let empty_suffix = dir.path().join("settings.json.another-tmp.");
        let no_dot = dir.path().join("settings.json.another-tmp");
        let other = dir.path().join("other.json.another-tmp.123.0.deadbeef");
        for p in [&stale, &empty_suffix, &no_dot, &other] {
            std::fs::write(p, "data").unwrap();
        }

        sweep_stale_temps(&target);

        assert!(!stale.exists());
        assert!(empty_suffix.exists());
        assert!(no_dot.exists());
        assert!(other.exists());
    }

    #[test]
    fn temp_prefix_bounds_long_ascii_multibyte_and_non_utf8_names() {
        // Short names stay verbatim.
        assert_eq!(
            temp_prefix(OsStr::new("settings.json")),
            OsString::from("settings.json.another-tmp.")
        );

        // 250-byte and 255-byte ASCII targets stay well within NAME_MAX (255).
        let long_a = format!("{}.json", "a".repeat(245));
        let long_b = format!("{}b.json", "a".repeat(244));
        let max_255 = format!("{}.json", "z".repeat(250));
        assert_eq!(long_a.len(), 250);
        assert_eq!(long_b.len(), 250);
        assert_eq!(max_255.len(), 255);

        let prefix_a = temp_prefix(OsStr::new(&long_a));
        let prefix_b = temp_prefix(OsStr::new(&long_b));
        let prefix_255 = temp_prefix(OsStr::new(&max_255));
        for p in [&prefix_a, &prefix_b, &prefix_255] {
            let len = p.as_encoded_bytes().len() + MAX_UNIQUE_SUFFIX_BYTES;
            assert!(
                len <= 255,
                "temp filename could exceed NAME_MAX: {len} bytes ({p:?})"
            );
            let s = p.to_str().expect("ASCII target must produce UTF-8 prefix");
            assert!(s.contains(TEMP_MARKER_LONG_UTF8));
            assert!(s.ends_with('.'));
            assert!(!s.ends_with(TEMP_MARKER_SHORT));
        }
        assert_ne!(
            prefix_a, prefix_b,
            "distinct long targets sharing a prefix must get distinct temp prefixes"
        );

        // Multibyte UTF-8 where byte 60 falls inside a 3-byte char (`1 + 19*3 = 58`, char spans 58..61).
        let multibyte = format!("x{}", "あ".repeat(83));
        assert_eq!(multibyte.len(), 250);
        assert!(!multibyte.is_char_boundary(MAX_TRUNCATED_NAME_BYTES));
        let prefix_mb = temp_prefix(OsStr::new(&multibyte));
        let prefix_mb_str = prefix_mb
            .to_str()
            .expect("UTF-8 target must produce UTF-8 prefix");
        assert!(prefix_mb_str.len() + MAX_UNIQUE_SUFFIX_BYTES <= 255);
        assert!(prefix_mb_str.contains(TEMP_MARKER_LONG_UTF8));
        assert!(prefix_mb_str.ends_with('.'));
        assert!(!prefix_mb_str.ends_with(TEMP_MARKER_SHORT));

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let raw = vec![0xffu8; 250];
            let prefix_raw = temp_prefix(OsStr::from_bytes(&raw));
            assert!(prefix_raw.as_encoded_bytes().len() + MAX_UNIQUE_SUFFIX_BYTES <= 255);
            let prefix_raw_str = prefix_raw
                .to_str()
                .expect("non-UTF-8 long target must produce ASCII prefix");
            assert!(prefix_raw_str.starts_with(TEMP_MARKER_LONG_RAW));
            assert!(prefix_raw_str.ends_with('.'));
            assert!(!prefix_raw_str.ends_with(TEMP_MARKER_SHORT));
        }
    }

    #[test]
    fn write_file_and_sweep_handle_250_byte_target_names() {
        let dir = tempfile::tempdir().unwrap();
        let target_name = format!("{}.json", "a".repeat(245));
        let sibling_name = format!("{}b.json", "a".repeat(244));
        assert_eq!(target_name.len(), 250);
        assert_eq!(sibling_name.len(), 250);

        let target = dir.path().join(&target_name);
        let target_prefix = temp_prefix(OsStr::new(&target_name));
        let sibling_prefix = temp_prefix(OsStr::new(&sibling_name));

        let mut stale_name = target_prefix.clone();
        stale_name.push("99999.0.deadbeef");
        let stale = dir.path().join(&stale_name);

        let mut sibling_stale_name = sibling_prefix;
        sibling_stale_name.push("99999.0.deadbeef");
        let sibling_stale = dir.path().join(&sibling_stale_name);

        let mut backup_name = target_prefix.clone();
        backup_name.push("backup");
        let backup = dir.path().join(&backup_name);

        let empty_suffix = dir.path().join(&target_prefix);
        let unrelated = dir.path().join("keep.txt");
        for p in [&stale, &sibling_stale, &backup, &empty_suffix, &unrelated] {
            std::fs::write(p, "data").unwrap();
        }

        let mut dir_name = target_prefix.clone();
        dir_name.push("77777.0.feedface");
        let stale_dir = dir.path().join(&dir_name);
        std::fs::create_dir(&stale_dir).unwrap();

        #[cfg(unix)]
        let (victim, stale_link) = {
            let victim = dir.path().join("victim.txt");
            std::fs::write(&victim, "do-not-touch").unwrap();
            let mut link_name = target_prefix;
            link_name.push("66666.0.badc0ffe");
            let link = dir.path().join(&link_name);
            std::os::unix::fs::symlink(&victim, &link).unwrap();
            (victim, link)
        };

        write_file(&target, b"{\"ok\":true}\n", Some(0o600), |_| Ok(())).unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "{\"ok\":true}\n");
        assert!(
            !stale.exists(),
            "stale temp for 250-byte target should be swept"
        );
        assert!(
            sibling_stale.exists(),
            "stale temp for distinct 250-byte sibling target must remain"
        );
        assert!(backup.exists(), "malformed suffix backup must remain");
        assert!(empty_suffix.exists(), "empty suffix must remain");
        assert!(unrelated.exists(), "unrelated file must remain");
        assert!(stale_dir.is_dir(), "directory matching prefix must remain");

        #[cfg(unix)]
        {
            assert!(std::fs::symlink_metadata(&stale_link)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(std::fs::read_to_string(&victim).unwrap(), "do-not-touch");
        }
    }

    #[test]
    fn sweep_preserves_malformed_suffixes_and_user_backups() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("settings.json");

        let valid_stale = dir.path().join("settings.json.another-tmp.123.0.deadbeef");
        let valid_max = dir
            .path()
            .join("settings.json.another-tmp.4294967295.18446744073709551615.0123abcd");

        let preserved_names = [
            "settings.json.another-tmp.backup",
            "settings.json.another-tmp.stale",
            "settings.json.another-tmp.123.0.abcd",
            "settings.json.another-tmp.123.0.deadbee",
            "settings.json.another-tmp.123.0.deadbeef0",
            "settings.json.another-tmp.123.0.DEADBEEF",
            "settings.json.another-tmp.123.0.deadbeef.bak",
            "settings.json.another-tmp.123.0.deadbeef.",
            "settings.json.another-tmp.0123.0.deadbeef",
            "settings.json.another-tmp.123.01.deadbeef",
            "settings.json.another-tmp.+123.0.deadbeef",
            "settings.json.another-tmp.4294967296.0.deadbeef",
            "settings.json.another-tmp.123.18446744073709551616.deadbeef",
            "settings.json.another-tmp.123.deadbeef",
            "settings.json.another-tmp.",
        ];

        std::fs::write(&valid_stale, "stale-1").unwrap();
        std::fs::write(&valid_max, "stale-2").unwrap();
        let preserved: Vec<PathBuf> = preserved_names
            .iter()
            .map(|name| {
                let path = dir.path().join(name);
                std::fs::write(&path, "keep").unwrap();
                path
            })
            .collect();

        sweep_stale_temps(&target);

        assert!(!valid_stale.exists(), "valid stale temp must be swept");
        assert!(
            !valid_max.exists(),
            "valid stale temp at u32/u64 max bounds must be swept"
        );
        for path in &preserved {
            assert!(
                path.exists(),
                "file with non-writer suffix must remain: {}",
                path.display()
            );
        }
    }

    #[test]
    fn long_and_short_targets_never_collide_on_temp_prefix_or_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let long_name = format!("{}.json", "a".repeat(245));
        assert_eq!(long_name.len(), 250);

        // Construct the exact 77-byte short sibling formed from the long target's
        // first 60 bytes + "." + 16-hex digest.
        let long_hash = &another_core::crypto::sha256_hex(long_name.as_bytes())[..HASH_HEX_LEN];
        let short_name = format!("{}.{long_hash}", &long_name[..MAX_TRUNCATED_NAME_BYTES]);
        assert_eq!(short_name.len(), 77);

        let long_prefix = temp_prefix(OsStr::new(&long_name));
        let short_prefix = temp_prefix(OsStr::new(&short_name));
        assert_ne!(
            long_prefix, short_prefix,
            "250-byte long target and 77-byte short sibling must never share a temp prefix"
        );

        let mut short_stale_name = short_prefix.clone();
        short_stale_name.push("99999.0.deadbeef");
        let short_stale = dir.path().join(&short_stale_name);

        let mut long_stale_name = long_prefix.clone();
        long_stale_name.push("88888.1.cafebabe");
        let long_stale = dir.path().join(&long_stale_name);

        std::fs::write(&short_stale, "short-sibling-temp").unwrap();
        std::fs::write(&long_stale, "long-target-temp").unwrap();

        let long_target = dir.path().join(&long_name);
        write_file(&long_target, b"long\n", Some(0o600), |_| Ok(())).unwrap();
        assert!(
            !long_stale.exists(),
            "long target's own stale temp must be swept"
        );
        assert!(
            short_stale.exists(),
            "writing 250-byte target must not delete 77-byte short sibling's temp"
        );

        // Also check the reverse direction: writing the short sibling leaves a long temp alone.
        std::fs::write(&long_stale, "long-target-temp-2").unwrap();
        let short_target = dir.path().join(&short_name);
        write_file(&short_target, b"short\n", Some(0o600), |_| Ok(())).unwrap();
        assert!(
            !short_stale.exists(),
            "short target's own stale temp must be swept"
        );
        assert!(
            long_stale.exists(),
            "writing 77-byte short target must not delete 250-byte long sibling's temp"
        );

        // Even a crafted short target embedding the long marker cannot share a prefix or sweep.
        let crafted_short = format!("{}{TEMP_MARKER_LONG_UTF8}{long_hash}", &long_name[..40]);
        assert!(crafted_short.len() <= MAX_INLINE_NAME_BYTES);
        let crafted_prefix = temp_prefix(OsStr::new(&crafted_short));
        assert_ne!(long_prefix, crafted_prefix);
        let mut crafted_stale_name = crafted_prefix;
        crafted_stale_name.push("77777.2.facefeed");
        let crafted_stale = dir.path().join(&crafted_stale_name);
        std::fs::write(&crafted_stale, "crafted-short-temp").unwrap();
        sweep_stale_temps(&long_target);
        assert!(
            crafted_stale.exists(),
            "sweeping long target must not delete crafted short target's temp"
        );
    }
}
