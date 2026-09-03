//! Client side: the §4.1 open sequence and the server verification that MUST
//! precede the first write.

use std::io;
use std::ptr::{null, null_mut};
use std::sync::Arc;

use windows_sys::Win32::Foundation::{
    GetLastError, LocalFree, ERROR_PIPE_BUSY, ERROR_SUCCESS, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
use windows_sys::Win32::Security::{
    IsWellKnownSid, WinBuiltinAdministratorsSid, WinLocalSystemSid, OWNER_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, SECURITY_IDENTIFICATION,
    SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Pipes::{GetNamedPipeServerProcessId, WaitNamedPipeW};

use crate::server::to_wide;
use crate::sid;
use crate::{Duplex, Pipe, PipeReader, PipeWriter};

/// Who owns the pipe object the client just opened.
///
/// §4.1's production rule is SYSTEM only. The other two exist for the M0/M1
/// console-mode service: an unelevated `--walk` service creates the pipe as
/// the user, and an elevated `--mft` service creates it with its token's
/// default owner, which for an elevated administrator token is the
/// Administrators group, not the user. Neither widens the threat model —
/// only that same user or an administrator could have created the pipe, and
/// both already own the session — but both are logged as dev mode so a
/// production install that is not running as SYSTEM stands out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerOwner {
    /// `S-1-5-18`: the installed service (§4.1).
    System,
    /// `S-1-5-32-544`: an elevated console-mode service.
    Administrators,
    /// This process's own user: an unelevated console-mode service.
    CurrentUser,
}

impl ServerOwner {
    pub fn is_dev_mode(self) -> bool {
        self != ServerOwner::System
    }
}

/// What the verification learned about the server before the first write.
#[derive(Clone, Copy, Debug)]
pub struct ServerInfo {
    pub pid: u32,
    pub owner: ServerOwner,
}

/// A connected, verified client end.
#[derive(Debug)]
pub struct Client {
    pub pipe: Arc<Pipe>,
    pub server: ServerInfo,
}

impl Client {
    pub fn split(self) -> io::Result<(PipeReader, PipeWriter)> {
        self.pipe.split()
    }

    pub fn duplex(self) -> io::Result<Duplex> {
        let (reader, writer) = self.pipe.split()?;
        Ok(Duplex { reader, writer })
    }
}

/// Open `name` per §4.1 and verify the server before returning, so that the
/// caller's first write (the `Hello`) is the pipe's first write.
///
/// Open: `dwDesiredAccess = CLIENT_PIPE_ACCESS` (never `GENERIC_WRITE`),
/// `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION`, `FILE_FLAG_OVERLAPPED`;
/// on `ERROR_PIPE_BUSY` wait 100 ms via `WaitNamedPipeW` and retry up to 5
/// times. Verify: `GetNamedPipeServerProcessId` for the log, then the pipe
/// object's owner SID via `GetSecurityInfo`, accepted per [`ServerOwner`];
/// any other owner is refused with `ErrorKind::PermissionDenied` and a
/// security event in the log.
pub fn connect(name: &str) -> io::Result<Client> {
    let pipe = open(name)?;
    let server = verify_server(&pipe)?;
    Ok(Client { pipe, server })
}

fn open(name: &str) -> io::Result<Arc<Pipe>> {
    let wide = to_wide(name);
    let mut attempts = 0u32;
    loop {
        // SAFETY: `wide` is a valid NUL-terminated UTF-16 string; all other
        // arguments are plain values / null pointers permitted by the API.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                yspot_proto::CLIENT_PIPE_ACCESS,
                0,
                null(),
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION | FILE_FLAG_OVERLAPPED,
                null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            // SAFETY: freshly opened overlapped handle, owned here.
            return Ok(Arc::new(unsafe { Pipe::from_raw(handle) }));
        }
        // SAFETY: trivially safe thread-local read.
        let err = unsafe { GetLastError() };
        if err == ERROR_PIPE_BUSY && attempts < 5 {
            attempts += 1;
            // SAFETY: same valid pipe name; 100 ms timeout per §4.1.
            let _ = unsafe { WaitNamedPipeW(wide.as_ptr(), 100) };
            continue;
        }
        return Err(io::Error::from_raw_os_error(err as i32));
    }
}

/// §4.1 client hardening, before the first write.
pub fn verify_server(pipe: &Pipe) -> io::Result<ServerInfo> {
    let mut pid = 0u32;
    // SAFETY: valid client pipe handle; out-param is a valid slot.
    if unsafe { GetNamedPipeServerProcessId(pipe.raw(), &mut pid) } == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut owner: PSID = null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: valid handle; only the owner is requested, so the group/DACL/SACL
    // out-params may be null; `sd` receives a LocalAlloc'd descriptor that
    // `owner` points into. READ_CONTROL is part of CLIENT_PIPE_ACCESS
    // (FILE_GENERIC_READ carries STANDARD_RIGHTS_READ), so this cannot be
    // refused by our own open rights.
    let rc = unsafe {
        GetSecurityInfo(
            pipe.raw(),
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut sd,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    let verdict = if owner.is_null() {
        None
    } else {
        // SAFETY: `owner` points into `sd`, alive until the LocalFree below.
        unsafe { classify(owner) }
    };
    // SAFETY: `sd` came from GetSecurityInfo, released with LocalFree once.
    unsafe { LocalFree(sd as _) };

    match verdict {
        Some(owner) => Ok(ServerInfo { pid, owner }),
        None => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "pipe owner is neither SYSTEM, Administrators, nor this user (SPEC §4.1)",
        )),
    }
}

/// # Safety
/// `owner` must point at a valid SID for the duration of the call.
unsafe fn classify(owner: PSID) -> Option<ServerOwner> {
    // SAFETY: valid SID per the contract.
    if unsafe { IsWellKnownSid(owner, WinLocalSystemSid) } != 0 {
        return Some(ServerOwner::System);
    }
    // SAFETY: valid SID per the contract.
    if unsafe { IsWellKnownSid(owner, WinBuiltinAdministratorsSid) } != 0 {
        return Some(ServerOwner::Administrators);
    }
    // SAFETY: valid SID per the contract.
    if sid::current_user_sid().is_some_and(|me| unsafe { me.equals(owner) }) {
        return Some(ServerOwner::CurrentUser);
    }
    // SAFETY: valid SID per the contract.
    let who = unsafe { sid::sid_to_string_unchecked(owner) }.unwrap_or_else(|| "?".into());
    log::error!(
        "SECURITY EVENT: pipe is owned by {who}, not SYSTEM/Administrators/this user — \
         refusing to talk to it (SPEC §4.1 client hardening)"
    );
    None
}
