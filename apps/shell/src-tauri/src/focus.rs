//! §5.2 focus model: activate-on-summon, restore-previous-foreground-on-dismiss.
//!
//! The previous foreground HWND is stored as an `isize` (HWND is just a
//! kernel handle value; storing it does not imply it stays valid — we
//! re-validate with `IsWindow` before restoring).

use std::sync::atomic::{AtomicIsize, Ordering};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, IsWindow, SetForegroundWindow,
};

static PREV_HWND: AtomicIsize = AtomicIsize::new(0);

/// Record the current foreground window (call on show, before we take focus).
pub fn remember_foreground() {
    // SAFETY: GetForegroundWindow has no preconditions and may return null.
    let hwnd = unsafe { GetForegroundWindow() };
    PREV_HWND.store(hwnd as isize, Ordering::SeqCst);
}

/// Hand focus back to the recorded window if it still exists (§5.2 step 3).
/// The stored handle is consumed — a second call is a no-op.
///
/// Returns the window focus was actually handed to, so a caller that is about
/// to synthesise input can check it got there. Windows refuses
/// `SetForegroundWindow` under the foreground lock and activates whatever is
/// next in Z-order instead; that is a debug line here, but for the clipboard's
/// Ctrl+V it is the difference between pasting into the window the user meant
/// and pasting into whatever the OS picked.
pub fn restore_foreground() -> Option<isize> {
    let prev = PREV_HWND.swap(0, Ordering::SeqCst);
    if prev == 0 {
        return None;
    }
    let hwnd = prev as HWND;
    // SAFETY: IsWindow/SetForegroundWindow accept arbitrary handle values;
    // a stale handle fails harmlessly.
    unsafe {
        if IsWindow(hwnd) == 0 {
            return None;
        }
        if SetForegroundWindow(hwnd) == 0 {
            log::debug!("SetForegroundWindow(prev) refused");
            return None;
        }
    }
    Some(prev)
}

/// Foreground our own window after show (§5.2 step 1). Succeeds because the
/// hotkey press made this process last-input; log if it is ever refused.
pub fn force_foreground(hwnd_raw: isize) {
    if hwnd_raw == 0 {
        return;
    }
    // SAFETY: SetForegroundWindow accepts arbitrary handle values.
    unsafe {
        if SetForegroundWindow(hwnd_raw as HWND) == 0 {
            log::warn!(
                "SetForegroundWindow(self) refused — window shown without foreground (§5.2)"
            );
        }
    }
}
