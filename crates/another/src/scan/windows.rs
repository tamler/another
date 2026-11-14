//! Read-only process inspection on native Windows.

use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{LocalFree, FILETIME, HANDLE};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, GetProcessTimes, OpenProcess, OpenProcessToken,
    QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};

use super::ProcInfo;

// FILETIME counts 100 ns intervals from 1601; UNIX_EPOCH starts in 1970.
const EPOCH_OFFSET_TICKS: u64 = 116_444_736_000_000_000;
const TICKS_PER_SECOND: u64 = 10_000_000;

fn filetime_ticks(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

fn elapsed_seconds(created_ticks: u64, unix_ticks: u64) -> Option<i64> {
    let now_ticks = unix_ticks.checked_add(EPOCH_OFFSET_TICKS)?;
    i64::try_from(now_ticks.checked_sub(created_ticks)? / TICKS_PER_SECOND).ok()
}

fn open_process_with_access(pid: u32, access: u32) -> Option<OwnedHandle> {
    // SAFETY: OpenProcess takes a numeric PID, no inherited handle, and returns either
    // null or a new owned process handle. OwnedHandle closes it exactly once.
    let handle = unsafe { OpenProcess(access, 0, pid) };
    if handle.is_null() {
        None
    } else {
        Some(unsafe { OwnedHandle::from_raw_handle(handle) })
    }
}

fn open_process(pid: u32) -> Option<OwnedHandle> {
    open_process_with_access(pid, PROCESS_QUERY_LIMITED_INFORMATION)
}

fn token_sid(process: HANDLE) -> Option<String> {
    let mut token = std::ptr::null_mut();
    // SAFETY: process is an open process handle (or the current-process pseudo handle),
    // and token points to writable storage for the returned owned handle.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 || token.is_null() {
        return None;
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut bytes = 0u32;
    // SAFETY: the first call queries the buffer size; a null zero-length buffer is valid.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut bytes,
        );
    }
    if bytes < std::mem::size_of::<TOKEN_USER>() as u32 || bytes > 65_536 {
        return None;
    }
    // usize backing provides the alignment TOKEN_USER requires.
    let words = (bytes as usize).div_ceil(std::mem::size_of::<usize>());
    let mut buffer = vec![0usize; words];
    let ok = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast::<c_void>(),
            bytes,
            &mut bytes,
        )
    };
    if ok == 0 {
        return None;
    }
    // SAFETY: a successful TokenUser call populated an aligned TOKEN_USER at buffer start;
    // its SID remains valid until this buffer is dropped.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    if user.User.Sid.is_null() {
        return None;
    }
    let mut sid = std::ptr::null_mut();
    // SAFETY: the SID came from GetTokenInformation; sid receives a LocalFree allocation.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) } == 0 || sid.is_null() {
        return None;
    }
    // Windows limits string SID length. Bound the scan and reject malformed UTF-16.
    let mut len = 0usize;
    while len < 256 && unsafe { *sid.add(len) } != 0 {
        len += 1;
    }
    let result = if len < 256 {
        String::from_utf16(unsafe { std::slice::from_raw_parts(sid, len) }).ok()
    } else {
        None
    };
    unsafe { LocalFree(sid.cast()) };
    result.filter(|value| !value.is_empty())
}

pub(super) fn current_user() -> Option<String> {
    // SAFETY: GetCurrentProcess returns a valid pseudo handle for this process.
    token_sid(unsafe { GetCurrentProcess() })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessIdentity {
    pub created_ticks: u64,
    pub owner: String,
    pub image: String,
}

fn inspect_handle(handle: HANDLE) -> Option<(ProcessIdentity, i64)> {
    let mut exit = 0u32;
    // Treat any failed query or exited process as unavailable, never as authorized.
    if unsafe { GetExitCodeProcess(handle, &mut exit) } == 0 || exit != 259 {
        return None;
    }
    let user = token_sid(handle)?;
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user_time = FILETIME::default();
    if unsafe {
        GetProcessTimes(
            handle,
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user_time,
        )
    } == 0
    {
        return None;
    }
    let now = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_nanos()
            / 100,
    )
    .ok()?;
    let elapsed_seconds = elapsed_seconds(filetime_ticks(created), now)?;
    let mut path = vec![0u16; 32_768];
    let mut len = path.len() as u32;
    if unsafe { QueryFullProcessImageNameW(handle, 0, path.as_mut_ptr(), &mut len) } == 0
        || len == 0
    {
        return None;
    }
    let command = String::from_utf16(&path[..len as usize]).ok()?;
    if unsafe { GetExitCodeProcess(handle, &mut exit) } == 0 || exit != 259 {
        return None;
    }
    Some((
        ProcessIdentity {
            created_ticks: filetime_ticks(created),
            owner: user,
            image: command,
        },
        elapsed_seconds,
    ))
}

pub(super) fn process_identity(pid: u32) -> Option<ProcessIdentity> {
    let process = open_process(pid)?;
    inspect_handle(process.as_raw_handle()).map(|(identity, _)| identity)
}

pub(super) fn process_info(pid: u32) -> Option<ProcInfo> {
    let process = open_process(pid)?;
    let (identity, elapsed_seconds) = inspect_handle(process.as_raw_handle())?;
    Some(ProcInfo {
        pid,
        user: identity.owner,
        elapsed_seconds,
        command: identity.image,
    })
}

fn authorized_identity(
    actual: &ProcessIdentity,
    expected: &ProcessIdentity,
    current_user: Option<&str>,
) -> bool {
    actual == expected
        && current_user == Some(actual.owner.as_str())
        && !super::is_system(&actual.image)
}

/// Open and inspect the target again, then terminate that exact handle and wait for exit.
pub(super) fn terminate_exact(pid: u32, expected: &ProcessIdentity) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    let refused = || {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "process identity could not be verified",
        )
    };
    if pid <= 1 || pid == std::process::id() {
        return Err(refused());
    }
    let process = open_process_with_access(
        pid,
        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
    )
    .ok_or_else(refused)?;
    let (actual, _) = inspect_handle(process.as_raw_handle()).ok_or_else(refused)?;
    if !authorized_identity(&actual, expected, current_user().as_deref()) {
        return Err(refused());
    }
    // SAFETY: the validated handle identifies the exact process, not a reused PID.
    if unsafe { TerminateProcess(process.as_raw_handle(), 1) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: waiting on the retained process handle does not affect any other process.
    if unsafe { WaitForSingleObject(process.as_raw_handle(), 5_000) } != WAIT_OBJECT_0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "termination was not confirmed",
        ));
    }
    Ok(())
}

pub(super) fn alive_or_unknown(pid: u32) -> bool {
    let Some(process) = open_process(pid) else {
        return true;
    };
    let mut exit = 0u32;
    unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut exit) == 0 || exit == 259 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_rejects_future_and_overflowing_times() {
        assert_eq!(
            elapsed_seconds(EPOCH_OFFSET_TICKS, 5 * TICKS_PER_SECOND),
            Some(5)
        );
        assert_eq!(
            elapsed_seconds(EPOCH_OFFSET_TICKS + 20_000_000, TICKS_PER_SECOND),
            None
        );
        assert_eq!(elapsed_seconds(0, u64::MAX), None);
    }

    #[test]
    fn current_process_owner_matches_current_user() {
        let pid = std::process::id();
        let info = process_info(pid).expect("current process must be inspectable");
        assert_eq!(info.pid, pid);
        assert_eq!(Some(info.user), current_user());
        assert!(!info.command.is_empty());
        assert!(alive_or_unknown(pid));
    }

    #[test]
    fn current_process_cannot_be_terminated() {
        let pid = std::process::id();
        let identity = process_identity(pid).expect("current process must be inspectable");
        assert!(terminate_exact(pid, &identity).is_err());
    }

    #[test]
    fn changed_creation_or_owner_is_not_authorized() {
        let identity = ProcessIdentity {
            created_ticks: 10,
            owner: "S-1-1".into(),
            image: r"C:\Users\agent\worker.exe".into(),
        };
        assert!(authorized_identity(&identity, &identity, Some("S-1-1")));
        let mut changed = identity.clone();
        changed.created_ticks += 1;
        assert!(!authorized_identity(&changed, &identity, Some("S-1-1")));
        assert!(!authorized_identity(&identity, &identity, Some("S-1-2")));
        assert!(!authorized_identity(&identity, &identity, None));
    }
}
