//! Smoke test for §10 M0 flagged open item #1: does
//! `AddClipboardFormatListener` deliver `WM_CLIPBOARDUPDATE` to a
//! MESSAGE-ONLY window?
//!
//! Why it was flagged: §7.4's clipboard history wants a listener with no
//! visible window, and the documentation is ambiguous about message-only
//! windows — clipboard *viewer* chains (`SetClipboardViewer`) famously do not
//! work with them, format listeners are documented per-window with no
//! HWND_MESSAGE caveat either way. This settles it empirically: register a
//! listener on an `HWND_MESSAGE` window, set the clipboard from a second
//! thread, and require the notification to arrive.
//!
//! The user's clipboard text is saved first and restored after — a smoke test
//! must not eat whatever was on the clipboard. Non-text contents (files, an
//! image) cannot be round-tripped this simply and are lost; the runbook says
//! to run this one first, before anything worth keeping is on the clipboard.

use anyhow::{bail, Context, Result};
use windows_sys::Win32::Foundation::GlobalFree;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard,
    SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, KillTimer, PostQuitMessage,
    RegisterClassW, SetTimer, TranslateMessage, HWND_MESSAGE, MSG, WM_CLIPBOARDUPDATE, WM_TIMER,
    WNDCLASSW,
};

const CF_UNICODETEXT: u32 = 13;
const TIMEOUT_TIMER_ID: usize = 1;
const TIMEOUT_MS: u32 = 3000;

/// Set by the wndproc when WM_CLIPBOARDUPDATE arrives. Single-threaded pump,
/// plain static is fine but the atomic keeps it defensible.
static GOT_UPDATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_CLIPBOARDUPDATE => {
            GOT_UPDATE.store(true, std::sync::atomic::Ordering::SeqCst);
            // SAFETY: posts to this thread's own queue.
            unsafe { PostQuitMessage(0) };
            0
        }
        WM_TIMER if w == TIMEOUT_TIMER_ID => {
            // SAFETY: as above.
            unsafe { PostQuitMessage(1) };
            0
        }
        // SAFETY: standard fallthrough for an owned window.
        _ => unsafe { DefWindowProcW(hwnd, msg, w, l) },
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Read CF_UNICODETEXT off the (already open) clipboard, if present.
///
/// # Safety
/// The clipboard must be open and owned by this thread.
unsafe fn read_text_locked() -> Option<String> {
    // SAFETY: caller holds the clipboard open.
    let h = unsafe { GetClipboardData(CF_UNICODETEXT) };
    if h.is_null() {
        return None;
    }
    // SAFETY: CF_UNICODETEXT handles are global memory holding a
    // NUL-terminated UTF-16 string; lock/unlock bracket the read.
    unsafe {
        let p = GlobalLock(h) as *const u16;
        if p.is_null() {
            return None;
        }
        let mut len = 0usize;
        while *p.add(len) != 0 {
            len += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
        GlobalUnlock(h);
        Some(s)
    }
}

fn set_clipboard_text(text: &str) -> Result<()> {
    let w = wide(text);
    let bytes = w.len() * 2;
    // SAFETY: allocate movable global memory, copy the string in, hand the
    // handle to the clipboard (which then owns it — no free on success).
    // OpenClipboard is retried against the same ambient contention
    // `get_clipboard_text` documents.
    unsafe {
        // The replacement handle is built and filled BEFORE the clipboard is
        // opened or emptied: EmptyClipboard destroys the old contents, and
        // destroying them before the replacement exists turns any later
        // failure into data loss instead of a no-op.
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if h.is_null() {
            bail!("GlobalAlloc failed");
        }
        let p = GlobalLock(h) as *mut u16;
        if p.is_null() {
            GlobalFree(h);
            bail!("GlobalLock failed");
        }
        std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
        GlobalUnlock(h);

        let mut opened = false;
        for attempt in 0..10 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(30));
            }
            if OpenClipboard(std::ptr::null_mut()) != 0 {
                opened = true;
                break;
            }
        }
        if !opened {
            GlobalFree(h);
            bail!("OpenClipboard failed after retries");
        }
        EmptyClipboard();
        if SetClipboardData(CF_UNICODETEXT, h as _).is_null() {
            // On failure the handle is still OURS to free — the clipboard
            // takes ownership only on success.
            CloseClipboard();
            GlobalFree(h);
            bail!("SetClipboardData failed");
        }
        CloseClipboard();
    }
    Ok(())
}

fn get_clipboard_text() -> Option<String> {
    // Retried: the clipboard is a globally contended object, and on Win11 the
    // clipboard-history service opens it immediately after every update — a
    // single OpenClipboard racing that loses with ERROR_ACCESS_DENIED. That
    // contention is ambient reality, not the property under test.
    for attempt in 0..10 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        // SAFETY: open/read/close on this thread; read_text_locked's contract
        // is exactly this bracket.
        unsafe {
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                continue;
            }
            let s = read_text_locked();
            CloseClipboard();
            return s;
        }
    }
    None
}

/// Run the smoke test. Ok(()) means the listener fired for a message-only
/// window; Err explains which step failed.
pub fn run() -> Result<()> {
    let saved = get_clipboard_text();
    if saved.is_some() {
        println!("clipboard: saved existing text contents (will restore)");
    }

    let class = wide("YSpotM0ClipListener");
    let wc = WNDCLASSW {
        style: 0,
        lpfnWndProc: Some(wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        // SAFETY: null hinstance resolves to this module for window classes.
        hInstance: std::ptr::null_mut(),
        hIcon: std::ptr::null_mut(),
        hCursor: std::ptr::null_mut(),
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: class.as_ptr(),
    };
    // SAFETY: wc references live strings; class registration is process-wide.
    if unsafe { RegisterClassW(&wc) } == 0 {
        bail!("RegisterClassW failed");
    }
    // SAFETY: HWND_MESSAGE parent creates the message-only window under test.
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            std::ptr::null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        bail!("CreateWindowExW(HWND_MESSAGE) failed");
    }
    // SAFETY: hwnd is our live window. THE call under test.
    if unsafe { AddClipboardFormatListener(hwnd) } == 0 {
        bail!("AddClipboardFormatListener refused a message-only window");
    }
    // SAFETY: timer on our own window; killed on drop of the pump below.
    unsafe { SetTimer(hwnd, TIMEOUT_TIMER_ID, TIMEOUT_MS, None) };

    let probe = format!("yspot-m0-{}", std::process::id());
    {
        let probe = probe.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            if let Err(e) = set_clipboard_text(&probe) {
                eprintln!("clipboard writer thread: {e:#}");
            }
        });
    }

    // Pump until WM_CLIPBOARDUPDATE (quit 0) or the timeout timer (quit 1).
    // SAFETY: standard message pump over our own thread's queue.
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        KillTimer(hwnd, TIMEOUT_TIMER_ID);
    }

    let fired = GOT_UPDATE.load(std::sync::atomic::Ordering::SeqCst);
    let seen = get_clipboard_text();

    // Restore before judging, whatever happened.
    if let Some(prev) = saved {
        set_clipboard_text(&prev).context("restoring the previous clipboard text")?;
        println!("clipboard: previous text contents restored");
    }

    if !fired {
        bail!(
            "WM_CLIPBOARDUPDATE did not reach the message-only window within {TIMEOUT_MS} ms — \
             the §7.4 clipboard-history design cannot use HWND_MESSAGE on this build"
        );
    }
    if seen.as_deref() != Some(probe.as_str()) {
        bail!("listener fired but the clipboard did not hold the probe text");
    }
    println!(
        "clipboard: PASS — WM_CLIPBOARDUPDATE delivered to a message-only window \
         (AddClipboardFormatListener + HWND_MESSAGE is viable for §7.4)"
    );
    Ok(())
}
