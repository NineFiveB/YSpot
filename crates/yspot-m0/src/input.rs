//! Injected input and the QPC clock — the harness's side of the shared
//! timeline. `qpc()` immediately before `SendInput` is the keydown timestamp
//! §10 M0 specifies; the ETW session stamps everything else in the same unit.

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
    VK_ESCAPE, VK_MENU, VK_SPACE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetWindowThreadProcessId, IsWindowVisible, SystemParametersInfoW, SPIF_SENDCHANGE,
    SPI_SETFOREGROUNDLOCKTIMEOUT,
};

pub fn qpc() -> i64 {
    let mut v = 0i64;
    // SAFETY: out-pointer to a local; cannot fail on XP+.
    unsafe { QueryPerformanceCounter(&mut v) };
    v
}

pub fn qpf() -> i64 {
    let mut v = 0i64;
    // SAFETY: as above.
    unsafe { QueryPerformanceFrequency(&mut v) };
    v
}

pub fn ticks_to_ms(ticks: i64, freq: i64) -> f64 {
    ticks as f64 * 1000.0 / freq as f64
}

fn key(vk: u16, scan: u16, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> bool {
    // SAFETY: `inputs` is a valid array of INPUT for the duration of the call.
    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        )
    };
    sent as usize == inputs.len()
}

/// Type one character into the focused window, as the launcher's §5.7
/// capture-phase handler will see it: a real keydown, then keyup.
pub fn send_char(c: char) -> bool {
    let mut buf = [0u16; 2];
    let units = c.encode_utf16(&mut buf);
    let mut inputs = Vec::with_capacity(units.len() * 2);
    for &u in units.iter() {
        inputs.push(key(0, u, KEYEVENTF_UNICODE));
        inputs.push(key(0, u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
    }
    send(&inputs)
}

/// The §5.1 chord: Alt down, Space down, Space up, Alt up — matches
/// RegisterHotKey(MOD_ALT, VK_SPACE) exactly, injected or physical.
pub fn send_alt_space() -> bool {
    send(&[
        key(VK_MENU, 0, 0),
        key(VK_SPACE, 0, 0),
        key(VK_SPACE, 0, KEYEVENTF_KEYUP),
        key(VK_MENU, 0, KEYEVENTF_KEYUP),
    ])
}

pub fn send_escape() -> bool {
    send(&[key(VK_ESCAPE, 0, 0), key(VK_ESCAPE, 0, KEYEVENTF_KEYUP)])
}

/// Disable the foreground-lock timeout for this session, so the launcher's own
/// `SetForegroundWindow` on summon is never refused.
///
/// Why it is needed, learned the hard way: an earlier version parked focus on
/// the desktop before every injected Alt+Space (to keep the chord off a
/// console's system menu). Rapidly alternating `SetForegroundWindow(desktop)`
/// with the shell's `SetForegroundWindow(self)` tripped Windows' foreground
/// lock after ~5 cycles; from then on hotkeys queued undelivered and every
/// cycle censored — until the harness stopped, when the backlog drained in one
/// burst. Running each step in a hidden console removed the console-menu
/// problem the parking existed for, so the parking is gone; this replaces it,
/// clearing the lock timeout once so no rapid foreground change can arm it.
///
/// Best-effort and self-reverting is not attempted: the value is process-wide
/// and the harness is short-lived; a stale 0 until the next login is harmless.
pub fn unlock_foreground() {
    // SAFETY: SPI_SETFOREGROUNDLOCKTIMEOUT takes the new timeout in pvParam
    // (as a usize), not a pointer; 0 ms disables the lock.
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_SETFOREGROUNDLOCKTIMEOUT,
            0,
            std::ptr::null_mut::<usize>() as *mut _,
            SPIF_SENDCHANGE,
        )
    };
    if ok == 0 {
        log::debug!("unlock_foreground: SystemParametersInfoW refused");
    }
}

/// Whether the launcher window exists and is currently visible; `None` when
/// no window titled "YSpot" that belongs to a `yspot-shell` process exists
/// (shell not running).
///
/// The process check is what makes this safe to act on: `FindWindowW` matches
/// by exact title across every top-level window, and the harness injects
/// KEYSTROKES on the strength of this answer — misidentifying some other
/// window titled "YSpot" would type into whatever has focus.
pub fn launcher_visible() -> Option<bool> {
    let title: Vec<u16> = "YSpot".encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: NUL-terminated title; null class matches any.
    let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
    if hwnd.is_null() || !window_process_is_shell(hwnd) {
        return None;
    }
    // SAFETY: hwnd was just returned; a stale handle yields FALSE, acceptable.
    Some(unsafe { IsWindowVisible(hwnd) } != 0)
}

fn window_process_is_shell(hwnd: windows_sys::Win32::Foundation::HWND) -> bool {
    let mut pid = 0u32;
    // SAFETY: out-pointer to a local; a stale hwnd yields pid 0.
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid == 0 {
        return false;
    }
    // SAFETY: query-limited open of a pid we just resolved; handle closed on
    // every path below.
    let proc = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if proc.is_null() {
        // Cannot verify (e.g. an elevated impostor) — treat as not the shell.
        return false;
    }
    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    // SAFETY: valid handle, buffer, and in/out length.
    let ok = unsafe { QueryFullProcessImageNameW(proc, 0, buf.as_mut_ptr(), &mut len) } != 0;
    // SAFETY: proc was opened above.
    unsafe { CloseHandle(proc) };
    if !ok {
        return false;
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]).to_ascii_lowercase();
    path.ends_with("yspot-shell.exe") || path.ends_with("yspot.exe")
}
