//! Read-only process inspection on native Windows.

use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{LocalFree, FILETIME, HANDLE};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, GetProcessTimes, OpenProcess, OpenProcessToken,
    QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
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

fn open_process(pid: u32) -> Option<OwnedHandle> {
    // SAFETY: OpenProcess takes a numeric PID, no inherited handle, and returns either
    // null or a new owned process handle. OwnedHandle closes it exactly once.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        None
    } else {
        Some(unsafe { OwnedHandle::from_raw_handle(handle) })
    }
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

pub(super) fn process_info(pid: u32) -> Option<ProcInfo> {
    let process = open_process(pid)?;
    let handle = process.as_raw_handle();
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
    Some(ProcInfo {
        pid,
        user,
        elapsed_seconds,
        command,
    })
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
}
