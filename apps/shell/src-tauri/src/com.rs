//! COM apartment bookkeeping for threads that call shell APIs.
//!
//! Shell APIs (`ShellExecuteEx`, the `AppsFolder` enumeration, icon
//! extraction) may activate in-process shell extensions, which expect a
//! single-threaded apartment on the calling thread. Tauri command threads
//! come from a pool with no apartment of their own, so every shell call
//! site initializes one for the duration of the call.

use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
};

/// An STA initialization on the current thread, balanced on drop.
///
/// `RPC_E_CHANGED_MODE` (the thread already lives in an MTA) is not an error
/// for the callers here — shell calls still work from an MTA, only in-proc
/// extensions marshal — so it is treated as "nothing to uninitialize".
pub struct Apartment {
    initialized: bool,
}

impl Apartment {
    pub fn sta() -> Apartment {
        // SAFETY: documented-null reserved argument; the call is the
        // canonical per-thread COM initialization.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        // S_OK and S_FALSE (already initialized in this mode) both need a
        // matching CoUninitialize; RPC_E_CHANGED_MODE does not.
        Apartment {
            initialized: hr.is_ok(),
        }
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: balances the successful CoInitializeEx above, on the
            // same thread (the struct is !Send: it holds no Send-able data,
            // and callers keep it on the stack of the initializing thread).
            unsafe { CoUninitialize() };
        }
    }
}

/// Decode and free a COM-allocated wide string.
///
/// # Safety
/// `p` must be a NUL-terminated string allocated with `CoTaskMemAlloc` (what
/// shell property and display-name getters return), and must not be used
/// after this call.
pub unsafe fn take_cotaskmem_string(p: windows::core::PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: NUL-terminated per the contract.
    let s = unsafe { p.to_string() }.unwrap_or_default();
    // SAFETY: allocated by CoTaskMemAlloc per the contract; freed once.
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

/// NUL-terminated UTF-16 for `PCWSTR` arguments.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
