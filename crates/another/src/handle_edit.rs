//! A low-level edit through a caller-owned open file.
//!
//! This module does not open paths, authorize a caller, establish project scope,
//! acquire holds, lock out other writers, or make writes atomic. It is not an
//! admission route. A caller must establish those properties before using it.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

/// Upper bound for each complete preimage and replacement.
pub const MAX_EDIT_BYTES: usize = 1024 * 1024;

/// File-system identity and current hard-link count obtained from the open handle.
#[derive(Debug, PartialEq, Eq)]
pub struct HandleIdentity {
    pub volume: u64,
    pub file: u64,
    pub links: u64,
}

/// Read identity from the file object itself, without reopening its path.
#[cfg(unix)]
pub fn handle_identity(file: &File) -> io::Result<HandleIdentity> {
    use std::os::unix::fs::MetadataExt;
    let info = file.metadata()?;
    Ok(HandleIdentity {
        volume: info.dev(),
        file: info.ino(),
        links: info.nlink(),
    })
}

/// Read identity from the file object itself, without reopening its path.
#[cfg(windows)]
pub fn handle_identity(file: &File) -> io::Result<HandleIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `file` owns a live HANDLE and `info` points to initialized writable
    // storage of the exact type required by this Windows API.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(HandleIdentity {
        volume: u64::from(info.dwVolumeSerialNumber),
        file: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        links: u64::from(info.nNumberOfLinks),
    })
}

/// Unsupported platforms cannot establish an identity.
#[cfg(not(any(unix, windows)))]
pub fn handle_identity(_file: &File) -> io::Result<HandleIdentity> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "file identity unavailable",
    ))
}

/// Observe whether a path currently names the same single-linked file.
///
/// This is an observation, not a namespace lock: a path or an ancestor can change
/// after this returns. It must not be used by itself to authorize an edit.
pub fn path_names_open_file(file: &File, path: &Path) -> io::Result<bool> {
    let named = std::fs::symlink_metadata(path)?;
    if !named.file_type().is_file() {
        return Ok(false);
    }
    let opened = handle_identity(file)?;
    if opened.links != 1 {
        return Ok(false);
    }
    let current = File::open(path)?;
    Ok(handle_identity(&current)? == opened)
}

/// Whether a refusal occurred before writing or a write's outcome is unknown.
#[derive(Debug)]
pub enum EditError {
    /// Nothing was written by this operation.
    Unchanged(io::Error),
    /// Writing started; the file may be partially or fully changed.
    OutcomeUnknown(io::Error),
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unchanged(e) => write!(f, "edit refused before writing: {e}"),
            Self::OutcomeUnknown(e) => write!(f, "edit outcome unknown: {e}"),
        }
    }
}

impl std::error::Error for EditError {}

/// Replace a complete, bounded preimage through the same open file object.
///
/// This detects a changed preimage before starting to write. It does not exclude
/// concurrent writes between that check and the write; the caller must do so.
/// An `OutcomeUnknown` error must not be treated as a rollback.
pub fn replace_open_file(
    file: &mut File,
    expected: &[u8],
    replacement: &[u8],
) -> Result<(), EditError> {
    let refused =
        |message| EditError::Unchanged(io::Error::new(io::ErrorKind::InvalidInput, message));
    if expected.len() > MAX_EDIT_BYTES || replacement.len() > MAX_EDIT_BYTES {
        return Err(refused("edit exceeds the size limit"));
    }
    let metadata = file.metadata().map_err(EditError::Unchanged)?;
    if !metadata.is_file() || metadata.len() > MAX_EDIT_BYTES as u64 {
        return Err(refused("file is not a bounded regular file"));
    }
    let identity = handle_identity(file).map_err(EditError::Unchanged)?;
    if identity.links != 1 {
        return Err(refused("file must have exactly one hard link"));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(EditError::Unchanged)?;
    let mut actual = Vec::with_capacity(expected.len());
    (&mut *file)
        .take((MAX_EDIT_BYTES + 1) as u64)
        .read_to_end(&mut actual)
        .map_err(EditError::Unchanged)?;
    if actual != expected {
        return Err(refused("file contents changed"));
    }
    if handle_identity(file).map_err(EditError::Unchanged)? != identity {
        return Err(refused("file identity or hard-link count changed"));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(EditError::Unchanged)?;
    write_replacement(file, replacement)?;
    file.set_len(replacement.len() as u64)
        .map_err(EditError::OutcomeUnknown)?;
    file.sync_all().map_err(EditError::OutcomeUnknown)?;
    Ok(())
}

fn write_replacement(writer: &mut impl Write, replacement: &[u8]) -> Result<(), EditError> {
    writer
        .write_all(replacement)
        .map_err(EditError::OutcomeUnknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_write_is_reported_as_unknown() {
        struct PartialWriter(Vec<u8>);
        impl Write for PartialWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.0.is_empty() {
                    let count = bytes.len().min(2);
                    self.0.extend_from_slice(&bytes[..count]);
                    Ok(count)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "injected write failure",
                    ))
                }
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut output = PartialWriter(Vec::new());
        assert!(matches!(
            write_replacement(&mut output, b"after"),
            Err(EditError::OutcomeUnknown(_))
        ));
        assert_eq!(output.0, b"af");
    }
}
