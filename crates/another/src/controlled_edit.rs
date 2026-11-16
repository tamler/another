//! The controlled edit owns its checked handles through writing and settlement.
//! Resolution and final comparison are observations, not a namespace lock.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
#[cfg(unix)]
use std::path::Component;
use std::path::Path;

use crate::handle_edit::{handle_identity, replace_open_file};
use another_core::{controlled_sha256, ControlledEdit, Store, MAX_CONTROLLED_BYTES};

fn refused() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "controlled file resolution refused",
    )
}

#[cfg(unix)]
fn directory_errno() -> io::Result<*mut i32> {
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    {
        Ok(unsafe { libc::__error() })
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        Ok(unsafe { libc::__errno_location() })
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "linux",
        target_os = "android"
    )))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "directory error reporting is unsupported",
        ))
    }
}

struct CheckedFile {
    root: File,
    file: File,
    // Ancestors stay open for the lifetime of the checked object.
    _ancestors: Vec<File>,
    #[cfg(windows)]
    _windows_parent: another_core::windows_home::PinnedDirectory,
}

#[cfg(unix)]
fn open_child(parent: &File, name: &str, directory: bool) -> io::Result<File> {
    use std::ffi::{CStr, CString};
    use std::os::fd::{AsRawFd, FromRawFd};
    let encoded = CString::new(name).map_err(|_| refused())?;
    let errno = directory_errno()?;
    // Use an independent directory offset: dup shares offsets with the original.
    let dot = c".";
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            dot.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fdopendir takes ownership of this fresh directory descriptor on success.
    let dir = unsafe { libc::fdopendir(descriptor) };
    if dir.is_null() {
        unsafe { libc::close(descriptor) };
        return Err(io::Error::last_os_error());
    }
    let mut matches = 0;
    let mut exact = false;
    let mut count = 0;
    let mut scan_error = None;
    loop {
        // SAFETY: dir remains live until closed below. Copy each name before readdir.
        unsafe { *errno = 0 };
        let entry = unsafe { libc::readdir(dir) };
        if entry.is_null() {
            let code = unsafe { *errno };
            if code != 0 {
                scan_error = Some(io::Error::from_raw_os_error(code));
            }
            break;
        }
        count += 1;
        if count > 100_000 {
            break;
        }
        let bytes = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if let Ok(found) = std::str::from_utf8(bytes) {
            if found.to_lowercase() == name.to_lowercase() {
                matches += 1;
            }
            if found == name {
                exact = true;
            }
        }
    }
    unsafe { libc::closedir(dir) };
    if let Some(error) = scan_error {
        return Err(error);
    }
    if count > 100_000 || !exact || matches != 1 {
        return Err(refused());
    }
    let flags = libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if directory {
            libc::O_RDONLY | libc::O_DIRECTORY
        } else {
            libc::O_RDWR
        };
    // SAFETY: parent is live, encoded is NUL terminated, openat returns an owned fd.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), encoded.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn open_root(path: &Path) -> io::Result<(File, Vec<File>)> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut current = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")?;
    let mut ancestors = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                let next = open_child(&current, name.to_str().ok_or_else(refused)?, true)?;
                ancestors.push(current);
                current = next;
            }
            _ => return Err(refused()),
        }
    }
    Ok((current, ancestors))
}

#[cfg(unix)]
fn nested_project(directory: &File) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    let mut info = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            c".git".as_ptr(),
            info.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOENT) {
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(unix)]
fn resolve(edit: &ControlledEdit, retained_root: Option<&File>) -> io::Result<CheckedFile> {
    let (root, mut ancestors) = match retained_root {
        Some(root) => (root.try_clone()?, Vec::new()),
        None => open_root(Path::new(&edit.project))?,
    };
    let mut parent = root.try_clone()?;
    let parts: Vec<_> = edit.path.split('/').collect();
    for (index, part) in parts.iter().enumerate() {
        let directory = index + 1 < parts.len();
        let next = open_child(&parent, part, directory)?;
        if directory && nested_project(&next)? {
            return Err(refused());
        }
        ancestors.push(parent);
        parent = next;
    }
    if !parent.metadata()?.is_file() || handle_identity(&parent)?.links != 1 {
        return Err(refused());
    }
    if retained_root.is_none() {
        use std::os::fd::AsRawFd;
        // SAFETY: parent is an owned live descriptor. The lock lasts until it is closed.
        if unsafe { libc::flock(parent.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(CheckedFile {
        root,
        file: parent,
        _ancestors: ancestors,
    })
}

#[cfg(windows)]
fn resolve(edit: &ControlledEdit, retained_root: Option<&File>) -> io::Result<CheckedFile> {
    use another_core::windows_home::pin_directory;
    let mut parent = pin_directory(Path::new(&edit.project))?;
    let root = parent.file.try_clone()?;
    if let Some(previous) = retained_root {
        if handle_identity(previous)? != handle_identity(&root)? {
            return Err(refused());
        }
    }
    let parts: Vec<_> = edit.path.split('/').collect();
    for part in &parts[..parts.len() - 1] {
        parent = parent.child(std::ffi::OsStr::new(part))?;
        match std::fs::symlink_metadata(parent.path.join(".git")) {
            Ok(_) => return Err(refused()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    let file = parent.file(
        std::ffi::OsStr::new(parts.last().ok_or_else(refused)?),
        retained_root.is_none(),
        false,
    )?;
    if handle_identity(&file)?.links != 1 {
        return Err(refused());
    }
    Ok(CheckedFile {
        root,
        file,
        _ancestors: Vec::new(),
        _windows_parent: parent,
    })
}

#[cfg(not(any(unix, windows)))]
fn resolve(_edit: &ControlledEdit, _root: Option<&File>) -> io::Result<CheckedFile> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "controlled resolution is not implemented on this platform",
    ))
}

/// Cooperating-session edit, with no stable-path or later-alias guarantee.
pub fn preflight(edit: &ControlledEdit) -> anyhow::Result<()> {
    edit.validate()?;
    drop(resolve(edit, None)?);
    Ok(())
}

/// Cooperating-session edit, with no stable-path or later-alias guarantee.
pub fn execute(
    store: &Store,
    client: &str,
    call: &str,
    edit: &ControlledEdit,
) -> anyhow::Result<String> {
    execute_with(store, client, call, edit, || {})
}

fn execute_with(
    store: &Store,
    client: &str,
    call: &str,
    edit: &ControlledEdit,
    after_write: impl FnOnce(),
) -> anyhow::Result<String> {
    edit.validate()?;
    let mut checked = resolve(edit, None)?;
    let object = handle_identity(&checked.file)?;
    let mut preimage = Vec::new();
    (&mut checked.file)
        .take((MAX_CONTROLLED_BYTES + 1) as u64)
        .read_to_end(&mut preimage)?;
    if preimage.len() > MAX_CONTROLLED_BYTES || controlled_sha256(&preimage) != edit.expected_sha256
    {
        anyhow::bail!("controlled edit preimage does not match; nothing written");
    }
    let identity = format!("{}:{}", object.volume, object.file);
    let claim = store.claim_controlled_edit(client, call, edit, &identity)?;
    let result = replace_open_file(&mut checked.file, &preimage, edit.replacement.as_bytes());
    after_write();
    let verified = result.is_ok()
        && (|| -> io::Result<bool> {
            if handle_identity(&checked.file)? != object {
                return Ok(false);
            }
            let after = resolve(edit, Some(&checked.root))?;
            let (root_now, _) = open_root_for_comparison(Path::new(&edit.project))?;
            if handle_identity(&after.file)? != object
                || handle_identity(&root_now)? != handle_identity(&checked.root)?
            {
                return Ok(false);
            }
            checked.file.seek(SeekFrom::Start(0))?;
            let mut bytes = Vec::new();
            (&mut checked.file)
                .take((MAX_CONTROLLED_BYTES + 1) as u64)
                .read_to_end(&mut bytes)?;
            Ok(bytes == edit.replacement.as_bytes())
        })()
        .unwrap_or(false);
    if !matches!(store.finish_controlled_edit(&claim, verified), Ok(true)) {
        anyhow::bail!(
            "controlled edit outcome unknown; do not retry automatically; inspect admission {}",
            claim.admission_id()
        );
    }
    Ok(format!("confirmed {}: full write and sync to the claimed object; a second checked resolution observed the same object and root. Path stability and later aliases are not guaranteed.", claim.admission_id()))
}

#[cfg(unix)]
fn open_root_for_comparison(path: &Path) -> io::Result<(File, Vec<File>)> {
    open_root(path)
}

#[cfg(windows)]
fn open_root_for_comparison(path: &Path) -> io::Result<(File, Vec<File>)> {
    Ok((
        another_core::windows_home::pin_directory(path)?
            .file
            .try_clone()?,
        Vec::new(),
    ))
}

#[cfg(not(any(unix, windows)))]
fn open_root_for_comparison(_path: &Path) -> io::Result<(File, Vec<File>)> {
    Err(refused())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use another_core::Program;

    #[test]
    fn detached_root_after_write_settles_unknown_and_is_not_retried() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        std::fs::write(root.join("a.rs"), b"before").unwrap();
        let edit = ControlledEdit {
            project: root.to_str().unwrap().into(),
            path: "a.rs".into(),
            expected_sha256: controlled_sha256(b"before"),
            replacement: "after".into(),
        };
        let store = Store::open(&temp.path().join("home")).unwrap();
        store
            .session_actor(
                "claude",
                Program::Claude,
                &edit.project,
                None,
                "session",
                true,
            )
            .unwrap();
        let id = store
            .prepare_controlled_edit(
                "claude",
                Program::Claude,
                &edit.project,
                None,
                "session",
                "call",
                &edit,
            )
            .unwrap();
        let detached = temp.path().join("detached");
        assert!(execute_with(&store, "claude", "call", &edit, || {
            std::fs::rename(&root, &detached).unwrap();
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("a.rs"), b"replacement-tree").unwrap();
        })
        .is_err());
        assert_eq!(std::fs::read(detached.join("a.rs")).unwrap(), b"after");
        assert_eq!(
            std::fs::read(root.join("a.rs")).unwrap(),
            b"replacement-tree"
        );
        assert_eq!(store.unknown_controlled_edits().unwrap(), vec![id]);
        assert!(execute(&store, "claude", "call", &edit).is_err());
    }

    #[test]
    fn intermediate_links_nested_projects_and_case_aliases_are_refused() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        std::fs::create_dir(root.join("real")).unwrap();
        std::fs::write(root.join("real/a.rs"), b"before").unwrap();
        symlink("real", root.join("link")).unwrap();
        let mut edit = ControlledEdit {
            project: root.to_str().unwrap().into(),
            path: "link/a.rs".into(),
            expected_sha256: controlled_sha256(b"before"),
            replacement: "after".into(),
        };
        assert!(resolve(&edit, None).is_err());
        edit.path = "real/A.rs".into();
        assert!(resolve(&edit, None).is_err());
        edit.path = "real/a.rs".into();
        std::fs::create_dir(root.join("real/.git")).unwrap();
        assert!(resolve(&edit, None).is_err());
    }
}
