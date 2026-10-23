//! The Windows half of [`crate::write_private`] and
//! [`crate::verify_owner_only`].
//!
//! This is the only module in the workspace allowed `unsafe` (ADR-0067). The
//! Win32 security API has no safe Rust binding the workspace trusts, and every
//! call below is a raw `windows-sys` FFI call. The rules that keep it small
//! enough to audit in one sitting:
//!
//! * one FFI call or one raw-pointer read per `unsafe` block, each with a
//!   `SAFETY:` comment naming what makes it sound;
//! * every buffer Windows allocates goes into a [`LocalBuf`] the moment it
//!   exists, and every handle into an [`OwnedHandle`], so no early return
//!   leaks one;
//! * no pointer outlives the value it points into, and none crosses this
//!   module's boundary: the public surface is `Path` in, `File` or `()` out.
//!
//! The descriptor a new file gets is the SDDL string `D:P(A;;FA;;;<sid>)`: a
//! DACL (`D:`) that is protected from inheritance (`P`), with one entry that
//! allows (`A`) full file access (`FA`) to the current user's SID. Building it
//! from SDDL means one parser call instead of four ACL-construction calls with
//! hand-sized buffers.

#![allow(unsafe_code)]

use std::ffi::{c_void, OsStr};
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::ptr;

use windows_sys::core::{BOOL, PWSTR};
use windows_sys::Win32::Foundation::{
    LocalFree, ERROR_INSUFFICIENT_BUFFER, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    AclSizeInformation, EqualSid, GetAce, GetAclInformation, GetSecurityDescriptorControl,
    GetTokenInformation, TokenUser, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION,
    DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SE_DACL_PROTECTED,
    TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, CREATE_NEW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// `ACCESS_ALLOWED_ACE_TYPE` from `winnt.h`. `windows-sys` has it only under
/// the `Win32_System_SystemServices` feature, a module of several thousand
/// declarations, for one zero.
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

/// A buffer Windows allocated with `LocalAlloc`, freed on drop.
struct LocalBuf(*mut c_void);

impl Drop for LocalBuf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: every `LocalBuf` holds a pointer a Win32 call returned
            // as `LocalAlloc`ed and documented as the caller's to `LocalFree`,
            // and nothing else frees it.
            unsafe { LocalFree(self.0) };
        }
    }
}

/// `Err(last OS error)` when a `BOOL`-returning call failed.
fn check(ok: BOOL) -> io::Result<()> {
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// `s` as a NUL-terminated UTF-16 string, refusing an interior NUL, which
/// would silently cut the name short.
fn wide(s: &OsStr) -> io::Result<Vec<u16>> {
    let mut out: Vec<u16> = s.encode_wide().collect();
    if out.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a path or descriptor with a NUL in it",
        ));
    }
    out.push(0);
    Ok(out)
}

/// The SID this process runs as, inside the `TOKEN_USER` that holds it.
struct UserSid {
    /// `u64`s rather than bytes so the buffer is aligned for `TOKEN_USER`,
    /// whose first field is a pointer. The SID that pointer names lives
    /// later in this same buffer, so it moves with it.
    buf: Vec<u64>,
}

impl UserSid {
    fn current() -> io::Result<Self> {
        // SAFETY: no arguments; it returns a pseudo-handle for this process,
        // which is always valid and is never closed.
        let process = unsafe { GetCurrentProcess() };
        let mut raw: HANDLE = ptr::null_mut();
        // SAFETY: `process` is valid, and `raw` is a valid place to write the
        // token handle.
        check(unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut raw) })?;
        // SAFETY: `OpenProcessToken` succeeded, so `raw` is an open handle
        // this process owns and nothing else will close.
        let owned = unsafe { OwnedHandle::from_raw_handle(raw) };
        let token: HANDLE = owned.as_raw_handle();

        let mut len = 0u32;
        // SAFETY: a null buffer of length 0 is the documented way to ask how
        // large the buffer must be; `len` is a valid place to write it.
        let sized =
            unsafe { GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &raw mut len) };
        let error = io::Error::last_os_error();
        if sized != 0 || error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER.cast_signed()) {
            return Err(error);
        }
        let mut buf = vec![0u64; (len as usize).div_ceil(size_of::<u64>())];
        // SAFETY: `buf` is writable, at least `len` bytes long and 8-aligned,
        // which covers `TOKEN_USER`'s alignment.
        check(unsafe {
            GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &raw mut len)
        })?;
        Ok(Self { buf })
    }

    fn as_psid(&self) -> PSID {
        // SAFETY: `buf` holds the `TOKEN_USER` `GetTokenInformation` wrote,
        // and is aligned for it.
        unsafe { (*self.buf.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }

    /// The SID in its `S-1-5-21-…` string form, for SDDL.
    fn to_string_sid(&self) -> io::Result<String> {
        let mut text: PWSTR = ptr::null_mut();
        // SAFETY: `as_psid` is a valid SID for as long as `self` lives, and
        // `text` is a valid place to write the string's address.
        check(unsafe { ConvertSidToStringSidW(self.as_psid(), &raw mut text) })?;
        let owned = LocalBuf(text.cast());
        let text = owned.0.cast::<u16>().cast_const();
        let mut len = 0;
        // SAFETY: `ConvertSidToStringSidW` returned a NUL-terminated string,
        // so every index up to and including the NUL is in bounds.
        while unsafe { *text.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: the `len` units before the NUL are initialised and stay
        // allocated until the `LocalBuf` above drops, after this copy.
        let units = unsafe { std::slice::from_raw_parts(text, len) };
        Ok(String::from_utf16_lossy(units))
    }
}

/// A self-relative security descriptor parsed from SDDL, freed on drop.
fn descriptor_from_sddl(sddl: &str) -> io::Result<LocalBuf> {
    let sddl = wide(OsStr::new(sddl))?;
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `sddl` is NUL-terminated and outlives the call; `descriptor`
    // is a valid place to write the result; the size out-parameter is
    // optional and null.
    check(unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &raw mut descriptor,
            ptr::null_mut(),
        )
    })?;
    Ok(LocalBuf(descriptor))
}

/// `CREATE_NEW` `path` with a protected DACL that allows the current user
/// alone, and return it open for writing.
pub(crate) fn create_owner_only(path: &Path) -> io::Result<File> {
    let sid = UserSid::current()?.to_string_sid()?;
    let descriptor = descriptor_from_sddl(&format!("D:P(A;;FA;;;{sid})"))?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).expect("a 24-byte struct"),
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let name = wide(path.as_os_str())?;
    // SAFETY: `name` is NUL-terminated, `attributes` and the descriptor it
    // points to outlive the call, and the template handle is optional and
    // null. The sharing flags are the ones `std::fs::File` uses.
    let raw = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &raw const attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `CreateFileW` succeeded, so `raw` is an open file handle this
    // process owns and nothing else will close.
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    Ok(File::from(handle))
}

/// The error [`verify_owner_only`] returns when the protection is wrong.
fn not_owner_only(path: &Path, why: &str) -> io::Error {
    io::Error::other(format!("{} is not owner-only: {why}", path.display()))
}

/// Read back `path`'s DACL and check it is what [`create_owner_only`] sets.
///
/// The entry's SID is compared with `EqualSid`, not as SDDL text: SDDL
/// prints some accounts by alias (`LA` for the built-in Administrator), so a
/// text comparison against `S-1-5-21-…-500` would fail on a correct file.
pub(crate) fn verify_owner_only(path: &Path) -> io::Result<()> {
    let name = wide(path.as_os_str())?;
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `name` is NUL-terminated; `dacl` and `descriptor` are valid
    // places to write; the owner, group and SACL out-parameters are optional
    // and null because only the DACL is requested.
    let status = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &raw mut dacl,
            ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status.cast_signed()));
    }
    // `dacl` points into this buffer, so it is only read while this lives.
    let descriptor = LocalBuf(descriptor);
    if dacl.is_null() {
        return Err(not_owner_only(
            path,
            "it has a null DACL, which allows everyone everything",
        ));
    }

    let mut control = 0u16;
    let mut revision = 0u32;
    // SAFETY: `descriptor` is the valid descriptor returned above; `control`
    // and `revision` are valid places to write.
    check(unsafe {
        GetSecurityDescriptorControl(descriptor.0, &raw mut control, &raw mut revision)
    })?;
    if control & SE_DACL_PROTECTED == 0 {
        return Err(not_owner_only(
            path,
            "its DACL is not protected, so it inherits entries from its directory",
        ));
    }

    let mut size = ACL_SIZE_INFORMATION::default();
    // SAFETY: `dacl` is non-null and valid while `descriptor` lives; `size`
    // is a writable `ACL_SIZE_INFORMATION` of exactly the length passed.
    check(unsafe {
        GetAclInformation(
            dacl,
            ptr::from_mut(&mut size).cast(),
            u32::try_from(size_of::<ACL_SIZE_INFORMATION>()).expect("a 12-byte struct"),
            AclSizeInformation,
        )
    })?;
    if size.AceCount != 1 {
        return Err(not_owner_only(
            path,
            &format!("its DACL holds {} entries, not one", size.AceCount),
        ));
    }

    let mut ace: *mut c_void = ptr::null_mut();
    // SAFETY: `dacl` is valid and holds one entry, so index 0 exists; `ace`
    // is a valid place to write its address.
    check(unsafe { GetAce(dacl, 0, &raw mut ace) })?;
    // SAFETY: `GetAce` succeeded, so `ace` points at an entry inside the
    // DACL, and every entry starts with an `ACE_HEADER`.
    let header = unsafe { ace.cast::<ACE_HEADER>().read_unaligned() };
    if header.AceType != ACCESS_ALLOWED_ACE_TYPE {
        return Err(not_owner_only(
            path,
            &format!("its one entry has type {}, not allow", header.AceType),
        ));
    }
    let allowed = ace.cast::<ACCESS_ALLOWED_ACE>();
    // SAFETY: an entry of type `ACCESS_ALLOWED_ACE_TYPE` is laid out as an
    // `ACCESS_ALLOWED_ACE`, whose `Mask` follows the header.
    let mask = unsafe { ptr::addr_of!((*allowed).Mask).read_unaligned() };
    if mask != FILE_ALL_ACCESS {
        return Err(not_owner_only(
            path,
            &format!("its one entry grants {mask:#x}, not full control ({FILE_ALL_ACCESS:#x})"),
        ));
    }
    // The entry's SID starts at `SidStart` and runs to the end of the entry.
    // SAFETY: as above; this takes the field's address and reads nothing.
    let entry_sid: PSID = unsafe { ptr::addr_of_mut!((*allowed).SidStart) }.cast();
    let me = UserSid::current()?;
    // SAFETY: `entry_sid` is a valid SID inside the DACL, which `descriptor`
    // keeps alive, and `me.as_psid()` is valid while `me` lives.
    if unsafe { EqualSid(entry_sid, me.as_psid()) } == 0 {
        return Err(not_owner_only(
            path,
            &format!(
                "its one entry is for another account than the current user ({})",
                me.to_string_sid()?
            ),
        ));
    }
    Ok(())
}

/// Give `dir` a DACL that allows `Everyone` full control and passes that on
/// to every file and directory created in it: the widest a directory can be,
/// and wider than the `C:\` case in `docs/07-clients/desktop.md` §Windows.
#[cfg(test)]
pub(crate) fn grant_everyone_inheritable(dir: &Path) -> io::Result<()> {
    use windows_sys::Win32::Security::SetFileSecurityW;

    let descriptor = descriptor_from_sddl("D:(A;OICI;FA;;;WD)")?;
    let name = wide(dir.as_os_str())?;
    // SAFETY: `name` is NUL-terminated and `descriptor` is a valid
    // descriptor; both outlive the call.
    check(unsafe { SetFileSecurityW(name.as_ptr(), DACL_SECURITY_INFORMATION, descriptor.0) })
}
