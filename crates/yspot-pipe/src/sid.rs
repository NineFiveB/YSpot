//! Security identifiers: the current process user's SID, owned and comparable.

use std::ffi::c_void;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
    EqualSid, GetLengthSid, GetTokenInformation, TokenUser, PSID, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// An owned SID (its bytes copied out of whatever buffer the API returned).
#[derive(Clone, Debug)]
pub struct Sid(Vec<u8>);

impl Sid {
    /// Copy the SID `p` points at.
    ///
    /// # Safety
    /// `p` must point at a valid SID for the duration of the call.
    pub unsafe fn copy_from(p: PSID) -> Sid {
        // SAFETY: the caller guarantees a valid SID; GetLengthSid reports its
        // byte length.
        let len = unsafe { GetLengthSid(p) } as usize;
        // SAFETY: `p` is valid for `len` bytes.
        let bytes = unsafe { std::slice::from_raw_parts(p as *const u8, len) };
        Sid(bytes.to_vec())
    }

    pub fn as_psid(&self) -> PSID {
        self.0.as_ptr() as PSID
    }

    /// Equality under `EqualSid` (the authority on SID identity).
    ///
    /// # Safety
    /// `other` must point at a valid SID for the duration of the call.
    pub unsafe fn equals(&self, other: PSID) -> bool {
        // SAFETY: self is a valid SID; the caller vouches for `other`.
        unsafe { EqualSid(self.as_psid(), other) != 0 }
    }

    /// SDDL string form (`S-1-5-...`).
    pub fn to_string_sid(&self) -> Option<String> {
        sid_to_string(self.as_psid())
    }
}

/// SDDL string form of an arbitrary SID pointer.
///
/// # Safety
/// `sid` must point at a valid SID for the duration of the call.
pub unsafe fn sid_to_string_unchecked(sid: PSID) -> Option<String> {
    sid_to_string(sid)
}

fn sid_to_string(sid: PSID) -> Option<String> {
    let mut wide: *mut u16 = null_mut();
    // SAFETY: `sid` is valid for the call; `wide` receives a LocalAlloc'd string.
    if unsafe { ConvertSidToStringSidW(sid, &mut wide) } == 0 || wide.is_null() {
        return None;
    }
    let mut n = 0usize;
    // SAFETY: `wide` is a NUL-terminated string from the API.
    while unsafe { *wide.add(n) } != 0 {
        n += 1;
    }
    // SAFETY: `n` units precede the terminator.
    let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(wide, n) });
    // SAFETY: LocalFree is the documented release for this allocation.
    unsafe { LocalFree(wide as _) };
    Some(s)
}

/// The user SID of this process's primary token.
pub fn current_user_sid() -> Option<Sid> {
    let mut token: HANDLE = null_mut();
    // SAFETY: pseudo-handle for the current process; out-param is a valid slot.
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if ok == 0 {
        return None;
    }
    let result = token_user_sid(token);
    // SAFETY: `token` is a real handle we opened; closed exactly once.
    unsafe { CloseHandle(token) };
    result
}

fn token_user_sid(token: HANDLE) -> Option<Sid> {
    let mut len = 0u32;
    // First call sizes the buffer; failing with anything other than "too
    // small" means we cannot ask, so give up rather than guess.
    // SAFETY: null buffer with a zero length is the documented sizing call.
    unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len) };
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len as usize];
    // SAFETY: `buf` is `len` bytes, which is what the sizing call asked for.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr() as *mut c_void,
            len,
            &mut len,
        )
    };
    if ok == 0 {
        return None;
    }
    // SAFETY: on success the buffer holds a TOKEN_USER whose SID pointer aims
    // into that same buffer, which outlives this read.
    let sid = unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid };
    // SAFETY: `sid` points into `buf`, still alive.
    Some(unsafe { Sid::copy_from(sid) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_user_sid_is_a_user_sid() {
        let sid = current_user_sid().expect("process token user SID");
        let s = sid.to_string_sid().expect("string form");
        assert!(s.starts_with("S-1-5-"), "unexpected SID {s}");
        // SAFETY: comparing a valid SID with itself.
        assert!(unsafe { sid.equals(sid.as_psid()) });
    }
}
