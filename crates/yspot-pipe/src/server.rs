//! Server side: instance creation with the §4.1 flags, overlapped accept, and
//! the SDDL → security-descriptor conversion.

use std::ffi::c_void;
use std::io;
use std::ptr::{null, null_mut};
use std::sync::Arc;

use windows_sys::Win32::Foundation::{
    GetLastError, LocalFree, ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_PIPE_CONNECTED,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW};
use windows_sys::Win32::System::IO::GetOverlappedResult;

use crate::handle::Event;
use crate::Pipe;

// ABI-stable Win32 constants, defined locally so windows-sys module placement
// cannot break the build. Values are from winbase.h and are frozen ABI.
const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
const PIPE_WAIT: u32 = 0x0000_0000;
const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x0000_0008;
const PIPE_UNLIMITED_INSTANCES: u32 = 255;
const SDDL_REVISION_1: u32 = 1;
const PIPE_BUF_BYTES: u32 = 64 * 1024;

/// §4.1 pipe mode: byte type, byte read, blocking (per operation — the handle
/// itself is overlapped), remote clients rejected at the kernel.
pub const PIPE_MODE: u32 =
    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;

/// Create one listening instance of `name` (§4.1): `PIPE_ACCESS_DUPLEX |
/// FILE_FLAG_OVERLAPPED`, plus `FILE_FLAG_FIRST_PIPE_INSTANCE` when `first` —
/// the squat check, which the caller recognizes as `ERROR_ACCESS_DENIED`
/// through `raw_os_error()`.
pub fn create_instance(name: &str, sd: Option<&SecDesc>, first: bool) -> io::Result<Arc<Pipe>> {
    let wide = to_wide(name);
    let mut open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }

    let sa;
    let psa: *const SECURITY_ATTRIBUTES = match sd {
        Some(d) => {
            sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: d.ptr,
                bInheritHandle: 0,
            };
            &sa
        }
        None => null(),
    };

    // SAFETY: `wide` is a nul-terminated UTF-16 string; `sa` (when present)
    // and the descriptor it points to outlive the call.
    let h = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            open_mode,
            PIPE_MODE,
            PIPE_UNLIMITED_INSTANCES,
            PIPE_BUF_BYTES,
            PIPE_BUF_BYTES,
            0, // default timeout (used by WaitNamedPipe's default)
            psa,
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh overlapped instance handle, owned here.
    Ok(Arc::new(unsafe { Pipe::from_raw(h) }))
}

/// Wait for a client on a listening instance.
///
/// `Ok(())` also covers a client that connected and went away before this was
/// called (`ERROR_NO_DATA`): the first read then reports EOF and the session
/// ends normally, which is simpler than a second "already gone" outcome.
pub fn accept(pipe: &Pipe) -> io::Result<()> {
    let ev = Event::new()?;
    // SAFETY: OVERLAPPED is plain data with all-zero as the documented
    // initial state; only hEvent is set.
    let mut ov: windows_sys::Win32::System::IO::OVERLAPPED = unsafe { std::mem::zeroed() };
    ov.hEvent = ev.raw();
    // SAFETY: valid listening instance handle; `ov` lives on this frame and the
    // function does not return until the operation has completed.
    let ok = unsafe { ConnectNamedPipe(pipe.raw(), &mut ov) };
    if ok != 0 {
        // Not expected for an overlapped handle, but documented as "connected".
        return Ok(());
    }
    // SAFETY: trivial FFI call.
    let err = unsafe { GetLastError() };
    match err {
        ERROR_PIPE_CONNECTED | ERROR_NO_DATA => Ok(()),
        ERROR_IO_PENDING => {
            let mut n = 0u32;
            // SAFETY: `ov` is the pending operation's structure; bWait = TRUE.
            if unsafe { GetOverlappedResult(pipe.raw(), &ov, &mut n, 1) } != 0 {
                return Ok(());
            }
            // SAFETY: trivial FFI call.
            match unsafe { GetLastError() } {
                ERROR_PIPE_CONNECTED | ERROR_NO_DATA => Ok(()),
                e => Err(io::Error::from_raw_os_error(e as i32)),
            }
        }
        e => Err(io::Error::from_raw_os_error(e as i32)),
    }
}

/// Owned self-relative security descriptor from
/// `ConvertStringSecurityDescriptorToSecurityDescriptorW`; freed with LocalFree.
pub struct SecDesc {
    ptr: *mut c_void,
}

// SAFETY: the descriptor is an immutable LocalAlloc'd blob; nothing mutates it
// after conversion, and freeing happens exactly once on drop.
unsafe impl Send for SecDesc {}
unsafe impl Sync for SecDesc {}

impl SecDesc {
    /// Convert an SDDL string; `None` (with the OS error logged) if it does
    /// not parse.
    pub fn from_sddl(sddl: &str) -> Option<SecDesc> {
        let wide = to_wide(sddl);
        let mut sd: *mut c_void = null_mut();
        // SAFETY: `wide` is a valid nul-terminated UTF-16 buffer for the call's
        // duration; `sd` is an out-parameter receiving a LocalAlloc'd
        // descriptor; size out-param may be null.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                null_mut(),
            )
        };
        if ok == 0 || sd.is_null() {
            // SAFETY: trivial FFI call.
            let err = unsafe { GetLastError() };
            log::warn!(
                "ConvertStringSecurityDescriptorToSecurityDescriptorW failed (os error {err})"
            );
            None
        } else {
            Some(SecDesc { ptr: sd })
        }
    }
}

impl Drop for SecDesc {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            // SAFETY: `ptr` was allocated by the SDDL conversion API with
            // LocalAlloc; LocalFree is its documented release function.
            let _ = unsafe { LocalFree(self.ptr as _) };
        }
    }
}

pub(crate) fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_strings_are_nul_terminated() {
        let w = to_wide("ab");
        assert_eq!(w, vec![b'a' as u16, b'b' as u16, 0]);
        assert_eq!(to_wide(""), vec![0]);
    }

    #[test]
    fn pipe_mode_bits() {
        // Byte-type, byte-read, blocking, remote clients rejected (§4.1).
        assert_eq!(PIPE_MODE, 0x8);
    }

    #[test]
    fn sddl_conversion_round_trips_the_spec_descriptor() {
        assert!(SecDesc::from_sddl(yspot_proto::PIPE_SDDL_NO_OWNER).is_some());
        assert!(SecDesc::from_sddl("not an sddl string").is_none());
    }
}
