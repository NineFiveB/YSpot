//! Named-pipe server (SPEC §4.1).
//!
//! M0 DEVIATIONS from §4.1, deliberate and documented:
//! - Blocking I/O and a thread per connection; no `FILE_FLAG_OVERLAPPED` yet
//!   (that lands with the M1 service wrapper).
//! - The listening instance is re-armed on one thread after each accept; a
//!   client connecting in the gap sees `ERROR_PIPE_BUSY` and retries via
//!   `WaitNamedPipe` (retry behavior §4.1 already mandates for clients).
//! - Unelevated dev runs cannot assert the SDDL's `O:SY` owner (`CreateNamedPipeW`
//!   fails with `ERROR_INVALID_OWNER`); the pipe is then created with
//!   `PIPE_SDDL_NO_OWNER` — the same DACL and integrity label, owned by the
//!   running user — so it stays ACL-hardened and only the §4.1 client-side
//!   SYSTEM-owner check does not apply.
//!
//! Squat detection (§4.1) is honored: the first instance carries
//! `FILE_FLAG_FIRST_PIPE_INSTANCE`, and `ERROR_ACCESS_DENIED` on that create
//! logs a security event and refuses to start.

use std::ffi::c_void;
use std::fs::File;
use std::os::windows::io::FromRawHandle;
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_ACCESS_DENIED, ERROR_INVALID_OWNER,
    ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::session;
use crate::state::ServiceState;

// ABI-stable Win32 constants, defined locally so windows-sys module placement
// cannot break the build. Values are from winbase.h and are frozen ABI.
const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
const PIPE_TYPE_BYTE: u32 = 0x0000_0000;
const PIPE_READMODE_BYTE: u32 = 0x0000_0000;
const PIPE_WAIT: u32 = 0x0000_0000;
const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x0000_0008;
const PIPE_UNLIMITED_INSTANCES: u32 = 255;
const SDDL_REVISION_1: u32 = 1;
const PIPE_BUF_BYTES: u32 = 64 * 1024;

/// Serve the pipe forever. Returns only on a fatal startup error; exits the
/// process directly on squat detection (§4.1).
pub fn serve(state: Arc<ServiceState>) -> anyhow::Result<()> {
    let wide_name = to_wide(yspot_proto::PIPE_NAME);

    let mut secdesc = SecDesc::from_sddl(yspot_proto::PIPE_SDDL);
    if secdesc.is_none() {
        log::warn!(
            "PIPE_SDDL did not convert; pipe will use DEFAULT security — M0 DEV MODE ONLY, \
             the pipe is NOT ACL-hardened (SPEC §4.1). Do not ship this configuration."
        );
    }

    // First instance carries FILE_FLAG_FIRST_PIPE_INSTANCE — the squat check.
    // (Pointer hoisted out of the scrutinee so the arms below may reassign
    // `secdesc` without borrow-extension trouble.)
    let sd_ptr = secdesc.as_ref().map(|d| d.ptr);
    let mut handle = match create_instance(&wide_name, sd_ptr, true) {
        Ok(h) => h,
        Err(err) if err == ERROR_ACCESS_DENIED => squat_refusal(),
        // Only SYSTEM may assign SYSTEM as owner, so an unelevated dev run
        // fails the full descriptor with ERROR_INVALID_OWNER. Fall back to the
        // same DACL and integrity label without the owner/group prefix — still
        // ACL-hardened, only the owner differs.
        Err(err) if err == ERROR_INVALID_OWNER && secdesc.is_some() => {
            log::warn!(
                "CreateNamedPipeW with the SPEC §4.1 SDDL failed (ERROR_INVALID_OWNER): this \
                 process is not SYSTEM. Retrying with the same DACL minus the O:SY/G:SY prefix — \
                 the pipe stays ACL-hardened, but its owner is this user, so clients cannot \
                 perform the §4.1 SYSTEM-owner check. DEV MODE."
            );
            secdesc = dev_sddl()
                .as_deref()
                .and_then(SecDesc::from_sddl)
                .or_else(|| SecDesc::from_sddl(yspot_proto::PIPE_SDDL_NO_OWNER));
            match create_instance(&wide_name, secdesc.as_ref().map(|d| d.ptr), true) {
                Ok(h) => h,
                Err(e) if e == ERROR_ACCESS_DENIED => squat_refusal(),
                Err(e) => anyhow::bail!("CreateNamedPipeW (dev descriptor) failed: os error {e}"),
            }
        }
        Err(err) => anyhow::bail!("CreateNamedPipeW failed: os error {err}"),
    };
    log::info!("pipe server listening on {}", yspot_proto::PIPE_NAME);

    loop {
        // SAFETY: `handle` is a valid listening pipe-instance handle owned by
        // this loop; blocking (non-overlapped) connect, lpOverlapped = null.
        let ok = unsafe { ConnectNamedPipe(handle, null_mut()) };
        // SAFETY: trivial FFI call, reads the calling thread's last-error slot.
        let connected = ok != 0 || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
        if connected {
            // SAFETY: sole ownership of `handle` transfers into the File,
            // which closes it on drop; `handle` is not used again below.
            let file = unsafe { File::from_raw_handle(handle as _) };
            let st = state.clone();
            if let Err(e) = std::thread::Builder::new()
                .name("pipe-conn".into())
                .spawn(move || session::run(file, st))
            {
                // File drop closes the instance; client sees a disconnect.
                log::error!("connection thread spawn failed: {e}");
            }
        } else {
            // SAFETY: trivial FFI call.
            let err = unsafe { GetLastError() };
            log::debug!("ConnectNamedPipe failed (os error {err}); recycling instance");
            // SAFETY: `handle` is a valid handle owned here; this is its close.
            let _ = unsafe { CloseHandle(handle) };
        }

        // Re-arm a fresh listening instance — WITHOUT the first-instance flag
        // (§4.1: with it, every later create would itself fail ACCESS_DENIED).
        handle = loop {
            match create_instance(&wide_name, secdesc.as_ref().map(|d| d.ptr), false) {
                Ok(h) => break h,
                Err(e) => {
                    log::error!(
                        "re-arm CreateNamedPipeW failed (os error {e}); retrying in 200 ms"
                    );
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        };
    }
}

/// Dev-mode security descriptor: the §4.1 DACL plus full control for the user
/// this process is running as.
///
/// Without that extra ACE an unelevated service can create the FIRST pipe
/// instance and no others. `PIPE_SDDL_NO_OWNER` grants `GA` to SYSTEM and
/// Administrators — neither of which an ordinary dev process is — and gives
/// interactive users `0x12019B`, which deliberately withholds
/// `FILE_CREATE_PIPE_INSTANCE` so a client cannot stand up a rogue instance of
/// our name. Creating a second instance is checked against the existing pipe's
/// DACL, so the service matched only the client ACE and got
/// `ERROR_ACCESS_DENIED` forever: it served exactly one connection and then
/// spun. In production the service IS SYSTEM, so the SY ace covers it and this
/// path never runs.
///
/// Other interactive users keep the reduced mask, so this widens nothing for
/// anyone but the process itself.
fn dev_sddl() -> Option<String> {
    let sid = current_user_sid()?;
    Some(format!(
        "D:P(A;;GA;;;{sid})(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x12019B;;;IU)S:(ML;;NW;;;ME)"
    ))
}

/// SDDL string form of the process token's user SID.
fn current_user_sid() -> Option<String> {
    // SAFETY: pseudo-handle, not a real one to close; out-param is a valid slot.
    let mut token: HANDLE = null_mut();
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if ok == 0 {
        return None;
    }
    let mut len = 0u32;
    // First call sizes the buffer; failing with anything other than "too small"
    // means we cannot ask, so give up rather than guess.
    // SAFETY: null buffer with a zero length is the documented sizing call.
    unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len) };
    if len == 0 {
        // SAFETY: `token` is a valid handle we opened.
        unsafe { CloseHandle(token) };
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
    // SAFETY: `token` is a valid handle we opened; closed exactly once.
    unsafe { CloseHandle(token) };
    if ok == 0 {
        return None;
    }
    // SAFETY: on success the buffer holds a TOKEN_USER whose SID pointer aims
    // into that same buffer, which outlives this read.
    let sid = unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid };
    let mut wide: *mut u16 = null_mut();
    // SAFETY: `sid` is valid for the call; `wide` receives a LocalAlloc'd string.
    if unsafe { ConvertSidToStringSidW(sid, &mut wide) } == 0 || wide.is_null() {
        return None;
    }
    // SAFETY: `wide` is a NUL-terminated string from the API.
    let mut n = 0usize;
    while unsafe { *wide.add(n) } != 0 {
        n += 1;
    }
    // SAFETY: `n` units precede the terminator.
    let s = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(wide, n) });
    // SAFETY: LocalFree is the documented release for this allocation.
    unsafe { LocalFree(wide as _) };
    Some(s)
}

fn squat_refusal() -> ! {
    // §4.1: another process owns our name — log a security event, refuse to
    // start degraded.
    log::error!(
        "SECURITY EVENT: first CreateNamedPipeW(FILE_FLAG_FIRST_PIPE_INSTANCE) failed with \
         ERROR_ACCESS_DENIED — another process has squatted {}; refusing to start (SPEC §4.1)",
        yspot_proto::PIPE_NAME
    );
    std::process::exit(10);
}

fn create_instance(wide_name: &[u16], sd: Option<*mut c_void>, first: bool) -> Result<HANDLE, u32> {
    let mut open_mode = PIPE_ACCESS_DUPLEX;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    let pipe_mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;

    let sa;
    let psa: *const SECURITY_ATTRIBUTES = match sd {
        Some(p) => {
            sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: p,
                bInheritHandle: 0,
            };
            &sa
        }
        None => null(),
    };

    // SAFETY: `wide_name` is a nul-terminated UTF-16 string; `sa` (when
    // present) and the descriptor it points to outlive the call.
    let h = unsafe {
        CreateNamedPipeW(
            wide_name.as_ptr(),
            open_mode,
            pipe_mode,
            PIPE_UNLIMITED_INSTANCES,
            PIPE_BUF_BYTES,
            PIPE_BUF_BYTES,
            0, // default timeout (used by WaitNamedPipe's default)
            psa,
        )
    };
    if h == INVALID_HANDLE_VALUE {
        // SAFETY: trivial FFI call.
        Err(unsafe { GetLastError() })
    } else {
        Ok(h)
    }
}

/// Owned self-relative security descriptor from
/// `ConvertStringSecurityDescriptorToSecurityDescriptorW`; freed with LocalFree.
struct SecDesc {
    ptr: *mut c_void,
}

impl SecDesc {
    fn from_sddl(sddl: &str) -> Option<SecDesc> {
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

fn to_wide(s: &str) -> Vec<u16> {
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
        let mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
        assert_eq!(mode, 0x8);
    }
}
