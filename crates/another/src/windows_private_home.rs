//! Windows handle security observations. This is not a home or SQLite VFS gate.
//!
//! The caller must retain the exact handle and separately establish trusted ancestry,
//! local filesystem identity, durable publication and SQLite main/sidecar confinement.
//! These checks alone do not authorize an open, edit, migration, or identity operation.

#![cfg(windows)]

use std::ffi::c_void;
use std::io;
use std::os::windows::io::{AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::ptr;

use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    CreateWellKnownSid, EqualSid, GetAce, GetLengthSid, GetSecurityDescriptorControl,
    GetTokenInformation, IsValidAcl, IsValidSid, MapGenericMask, TokenUser,
    WinBuiltinAdministratorsSid, WinLocalSystemSid, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
    DACL_SECURITY_INFORMATION, GENERIC_MAPPING, OWNER_SECURITY_INFORMATION, PSID, SE_DACL_PRESENT,
    SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, GetFileType, GetVolumeInformationByHandleW,
    BY_HANDLE_FILE_INFORMATION, FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_TYPE_DISK,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

const FILE_PERSISTENT_ACLS: u32 = 0x0000_0008;
const ALLOWED_ACE_TYPE: u8 = 0;
const VALID_INHERIT_FLAGS: u8 = 0x1f;
const INHERIT_ONLY: u8 = 0x08;
const INHERITANCE_TARGETS: u8 = 0x03;
const NO_PROPAGATE: u8 = 0x04;
const SID_MAX_BYTES: usize = 68;
const ACE_LIMIT: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectKind {
    Directory,
    RegularFile,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandleEvidence {
    pub volume_serial: u32,
    pub file_index: u64,
}

fn refused() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "Windows handle security could not be verified",
    )
}

struct SecurityDescriptor(*mut c_void);

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: GetSecurityInfo returns one LocalFree-owned descriptor allocation.
            unsafe { LocalFree(self.0) };
        }
    }
}

struct Sid(Vec<usize>);

impl Sid {
    fn as_ptr(&self) -> PSID {
        self.0.as_ptr().cast_mut().cast()
    }

    fn well_known(kind: i32) -> io::Result<Self> {
        let mut words = vec![0usize; SID_MAX_BYTES.div_ceil(std::mem::size_of::<usize>())];
        let mut bytes = SID_MAX_BYTES as u32;
        // SAFETY: the aligned buffer has SID_MAX_BYTES writable bytes and a valid length pointer.
        if unsafe {
            CreateWellKnownSid(kind, ptr::null_mut(), words.as_mut_ptr().cast(), &mut bytes)
        } == 0
        {
            return Err(refused());
        }
        let sid = Self(words);
        // SAFETY: CreateWellKnownSid succeeded and wrote a SID into the live buffer.
        if bytes as usize > SID_MAX_BYTES || unsafe { IsValidSid(sid.as_ptr()) } == 0 {
            return Err(refused());
        }
        Ok(sid)
    }
}

fn current_user_sid() -> io::Result<Sid> {
    let mut raw = ptr::null_mut();
    // SAFETY: GetCurrentProcess is a valid pseudo-handle, and raw is writable.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw) } == 0 || raw.is_null()
    {
        return Err(refused());
    }
    // SAFETY: a successful OpenProcessToken returns a newly owned handle.
    let token = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut bytes = 0u32;
    // SAFETY: the first call requests the required buffer size.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            ptr::null_mut(),
            0,
            &mut bytes,
        )
    };
    if (bytes as usize) < std::mem::size_of::<TOKEN_USER>() || bytes > 65_536 {
        return Err(refused());
    }
    let mut buffer = vec![0usize; (bytes as usize).div_ceil(std::mem::size_of::<usize>())];
    // SAFETY: the aligned buffer contains at least `bytes` writable bytes.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        )
    } == 0
    {
        return Err(refused());
    }
    // SAFETY: a successful TokenUser query populates the aligned TOKEN_USER prefix.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let begin = buffer.as_ptr() as usize;
    let end = begin
        .checked_add(buffer.len() * std::mem::size_of::<usize>())
        .ok_or_else(refused)?;
    let sid_at = sid as usize;
    if sid.is_null() || sid_at < begin || sid_at.checked_add(8).is_none_or(|n| n > end) {
        return Err(refused());
    }
    // SAFETY: TOKEN_USER is a system-returned structure and its SID begins within the buffer.
    if unsafe { IsValidSid(sid) } == 0 {
        return Err(refused());
    }
    let length = unsafe { GetLengthSid(sid) } as usize;
    if !(8..=SID_MAX_BYTES).contains(&length) || sid_at.checked_add(length).is_none_or(|n| n > end)
    {
        return Err(refused());
    }
    let mut result = vec![0usize; length.div_ceil(std::mem::size_of::<usize>())];
    // SAFETY: both allocations are valid for `length` bytes and do not overlap.
    unsafe { ptr::copy_nonoverlapping(sid.cast::<u8>(), result.as_mut_ptr().cast::<u8>(), length) };
    Ok(Sid(result))
}

fn mapped_mask(mut mask: u32) -> u32 {
    let mapping = GENERIC_MAPPING {
        GenericRead: FILE_GENERIC_READ,
        GenericWrite: FILE_GENERIC_WRITE,
        GenericExecute: FILE_GENERIC_EXECUTE,
        GenericAll: FILE_ALL_ACCESS,
    };
    // SAFETY: both pointers refer to live properly aligned values.
    unsafe { MapGenericMask(&mut mask, &mapping) };
    mask
}

/// Examine an already-open file or directory handle. `required_access` is a file access mask.
/// The handle must have been opened without following reparse points and with READ_CONTROL.
/// No pathname, ancestor, SQLite sidecar, network locality, or race-safety claim is made.
pub fn inspect_private_handle(
    handle: BorrowedHandle<'_>,
    kind: ObjectKind,
    required_access: u32,
) -> io::Result<HandleEvidence> {
    let h = handle.as_raw_handle();
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `h` is borrowed and live, and all output pointers below are valid.
    if unsafe { GetFileType(h) } != FILE_TYPE_DISK
        || unsafe { GetFileInformationByHandle(h, &mut info) } == 0
    {
        return Err(refused());
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(refused());
    }
    let is_directory = info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    if is_directory != (kind == ObjectKind::Directory)
        || (!is_directory && info.nNumberOfLinks != 1)
    {
        return Err(refused());
    }
    let mut volume_flags = 0u32;
    if unsafe {
        GetVolumeInformationByHandleW(
            h,
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut volume_flags,
            ptr::null_mut(),
            0,
        )
    } == 0
        || volume_flags & FILE_PERSISTENT_ACLS == 0
    {
        return Err(refused());
    }

    let user = current_user_sid()?;
    let system = Sid::well_known(WinLocalSystemSid)?;
    let admins = Sid::well_known(WinBuiltinAdministratorsSid)?;
    let mut owner: PSID = ptr::null_mut();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut raw_descriptor = ptr::null_mut();
    let result = unsafe {
        GetSecurityInfo(
            h,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut raw_descriptor,
        )
    };
    let descriptor = SecurityDescriptor(raw_descriptor);
    if result != 0 || descriptor.0.is_null() || owner.is_null() || dacl.is_null() {
        return Err(refused());
    }
    let mut control = 0u16;
    let mut revision = 0u32;
    // SAFETY: pointers are from a successful GetSecurityInfo and live for descriptor's lifetime.
    if unsafe { IsValidSid(owner) } == 0
        || unsafe { EqualSid(owner, user.as_ptr()) } == 0
        || unsafe { IsValidAcl(dacl) } == 0
        || unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) } == 0
        || control & SE_DACL_PRESENT == 0
        || (is_directory && control & SE_DACL_PROTECTED == 0)
    {
        return Err(refused());
    }
    // SAFETY: IsValidAcl accepted this ACL and it remains owned by `descriptor`.
    let ace_count = unsafe { (*dacl).AceCount } as usize;
    if ace_count == 0 || ace_count > ACE_LIMIT {
        return Err(refused());
    }
    let mut user_access = 0u32;
    for index in 0..ace_count {
        let mut raw_ace: *mut c_void = ptr::null_mut();
        // SAFETY: GetAce indexes a valid ACL and writes one pointer into raw_ace.
        if unsafe { GetAce(dacl, index as u32, &mut raw_ace) } == 0 || raw_ace.is_null() {
            return Err(refused());
        }
        // SAFETY: GetAce returns an aligned ACE header within the valid ACL.
        let header = unsafe { &*raw_ace.cast::<ACE_HEADER>() };
        let flags = header.AceFlags;
        let size = usize::from(header.AceSize);
        if header.AceType != ALLOWED_ACE_TYPE
            || flags & !VALID_INHERIT_FLAGS != 0
            || (flags & (INHERIT_ONLY | NO_PROPAGATE) != 0 && flags & INHERITANCE_TARGETS == 0)
            || size < 16
        {
            return Err(refused());
        }
        // SAFETY: this is a standard allowed ACE with enough bytes for its fixed prefix.
        let ace = unsafe { &*raw_ace.cast::<ACCESS_ALLOWED_ACE>() };
        let sid = (&ace.SidStart as *const u32).cast::<u8>();
        // SAFETY: a standard allowed ACE of at least 16 bytes contains the eight-byte SID header.
        let subauthorities = unsafe { *sid.add(1) } as usize;
        if subauthorities > 15 || 8 + subauthorities * 4 != size - 8 {
            return Err(refused());
        }
        let sid = sid.cast_mut().cast::<c_void>();
        if unsafe { IsValidSid(sid) } == 0 {
            return Err(refused());
        }
        let is_user = unsafe { EqualSid(sid, user.as_ptr()) } != 0;
        if !is_user
            && unsafe { EqualSid(sid, system.as_ptr()) } == 0
            && unsafe { EqualSid(sid, admins.as_ptr()) } == 0
        {
            return Err(refused());
        }
        let mask = mapped_mask(ace.Mask);
        if mask & !FILE_ALL_ACCESS != 0 {
            return Err(refused());
        }
        if is_user && flags & INHERIT_ONLY == 0 {
            user_access |= mask;
        }
    }
    let required = mapped_mask(required_access);
    if required == 0 || required & !FILE_ALL_ACCESS != 0 || user_access & required != required {
        return Err(refused());
    }
    Ok(HandleEvidence {
        volume_serial: info.dwVolumeSerialNumber,
        file_index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsHandle;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
    use windows_sys::Win32::Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL,
    };

    struct Fixture(PathBuf);

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_file(&self.0).unwrap();
        }
    }

    fn fixture(extra_ace: &str) -> (Fixture, OwnedHandle) {
        let user = current_user_sid().unwrap();
        let mut sid_string = ptr::null_mut();
        // SAFETY: user contains a valid SID and sid_string is writable.
        assert_ne!(
            unsafe { ConvertSidToStringSidW(user.as_ptr(), &mut sid_string) },
            0
        );
        let mut length = 0;
        // SAFETY: Windows returned a NUL-terminated SID string.
        unsafe {
            while *sid_string.add(length) != 0 {
                length += 1;
            }
        }
        // SAFETY: the previous loop found the string's terminator.
        let sid =
            String::from_utf16(unsafe { std::slice::from_raw_parts(sid_string, length) }).unwrap();
        // SAFETY: ConvertSidToStringSidW allocates its result with LocalAlloc.
        unsafe { LocalFree(sid_string.cast()) };
        let sddl = format!("O:{sid}D:P(A;;FA;;;{sid}){extra_ace}");
        let wide: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut raw = ptr::null_mut();
        // SAFETY: wide is NUL-terminated and raw is writable.
        assert_ne!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide.as_ptr(),
                    SDDL_REVISION_1,
                    &mut raw,
                    ptr::null_mut(),
                )
            },
            0
        );
        let descriptor = SecurityDescriptor(raw);
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "another-private-handle-{}-{nonce}",
            std::process::id()
        ));
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: inputs remain live for the call and the security descriptor is valid.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | READ_CONTROL,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                &attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                ptr::null_mut(),
            )
        };
        assert_ne!(
            handle,
            INVALID_HANDLE_VALUE,
            "{}",
            io::Error::last_os_error()
        );
        // SAFETY: CreateFileW returned a valid, newly owned handle.
        (Fixture(path), unsafe {
            OwnedHandle::from_raw_handle(handle)
        })
    }

    #[test]
    fn private_owner_acl_is_accepted_on_real_file() {
        let (fixture, handle) = fixture("");
        let evidence = inspect_private_handle(
            handle.as_handle(),
            ObjectKind::RegularFile,
            FILE_GENERIC_READ,
        )
        .unwrap();
        assert_ne!(evidence.file_index, 0);
        drop(handle);
        drop(fixture);
    }

    #[test]
    fn public_allow_and_deny_aces_are_refused() {
        for ace in ["(A;;FR;;;WD)", "(D;;FW;;;WD)"] {
            let (fixture, handle) = fixture(ace);
            assert!(inspect_private_handle(
                handle.as_handle(),
                ObjectKind::RegularFile,
                FILE_GENERIC_READ
            )
            .is_err());
            drop(handle);
            drop(fixture);
        }
    }

    #[test]
    fn mismatched_kind_and_invalid_requested_rights_are_refused() {
        let (fixture, handle) = fixture("");
        assert!(inspect_private_handle(
            handle.as_handle(),
            ObjectKind::Directory,
            FILE_GENERIC_READ
        )
        .is_err());
        assert!(inspect_private_handle(handle.as_handle(), ObjectKind::RegularFile, 0).is_err());
        drop(handle);
        drop(fixture);
    }
}
