//! Being summoned by an external hotkey daemon (SPEC.md §5.1 as amended).
//!
//! §5.1 originally required the shell to own its chord through `RegisterHotKey`.
//! That is still the default, and still the right default for a launcher
//! installed on its own. But a machine with a suite on it — YTile, YKeys, YSpot
//! — ends up with several processes each holding a piece of the keyboard, and
//! `RegisterHotKey` has no way to ask who owns what: the first process to claim
//! a chord wins and everyone else gets a bare `FALSE`. One daemon owning the
//! whole keyboard is the arrangement that can actually be reasoned about.
//!
//! So YSpot also accepts being *told* to appear. A message-only window listens
//! for one registered message; [`YKeys`](https://github.com/NineFiveB/YKeys)
//! posts it from the thread that received `WM_HOTKEY`.
//!
//! Why a message and not a command line: YKeys' ordinary bindings start a
//! process, and starting one costs 8 ms at the median and 20 ms at p95 on a
//! warm machine even for a program that does nothing — against §5.1's 50 ms
//! hotkey→visible budget, which is an M0 exit criterion. Posting a message
//! costs microseconds, so the amendment does not spend the gate.
//!
//! **The foreground hand-off is the load-bearing part.** Pressing the chord
//! makes YKEYS the process Windows will let take the foreground, not us; a
//! window shown without it appears and then silently does not have the
//! keyboard, which is §5.2's failure mode exactly. YKeys calls
//! `AllowSetForegroundWindow(our pid)` before posting. If that is ever missed,
//! [`crate::focus::force_foreground`] logs the refusal rather than leaving it
//! to be discovered by typing into nothing.
//!
//! This window is listening even when the shell registers its own chord: it
//! costs one thread, and a user who binds YKeys to a second verb (the clipboard
//! view, say) should not have to change a setting first.

use tauri::AppHandle;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW,
    RegisterWindowMessageW, TranslateMessage, HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE,
    WNDCLASSW,
};

/// The window class an external daemon looks for.
///
/// Part of the contract with anything that signals us — it is what the user
/// writes in `ykeys.json`, so it is as public as a command-line flag and
/// changing it breaks every config in the field.
pub const WINDOW_CLASS: &str = "YSpot.Signal";

/// The string both sides hand `RegisterWindowMessage`, which is how they agree
/// on a message id without sharing a header and without colliding with anyone's
/// `WM_APP` range.
pub const MESSAGE_NAME: &str = "YKeysSignal";

/// What the `wParam` codes mean. The numbers are the wire format — append
/// only, never renumber.
mod code {
    /// Show if hidden, dismiss if visible. The default, and what a bare
    /// `@signal:YSpot.Signal` sends.
    pub const TOGGLE: usize = 0;
    /// Show unconditionally: a chord that means "search", not "switch".
    pub const SHOW: usize = 1;
    pub const SETTINGS: usize = 2;
    pub const CLIPBOARD: usize = 3;
}

thread_local! {
    /// The handle the window procedure dispatches through. Thread-local for
    /// the same reason the clipboard listener's store is: the procedure runs
    /// on exactly the thread that made the window, so nothing has to cross
    /// threads and the type system can say so.
    static SIGNAL_APP: std::cell::RefCell<Option<AppHandle>> =
        const { std::cell::RefCell::new(None) };

    /// `RegisterWindowMessage`'s id for [`MESSAGE_NAME`]. Zero means the
    /// registration failed, and zero is never a valid message to match on.
    static SIGNAL_MESSAGE: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Start the listener on a thread of its own.
///
/// Its own thread, not the main one: this window needs a message loop that
/// runs for the life of the process, and the launcher's own loop belongs to
/// Tauri. It is deliberately separate from the clipboard listener's window
/// too — capture can be paused (§7.4), and being able to summon the launcher
/// must not depend on a feature the user switched off.
pub fn spawn(app: AppHandle) {
    let spawned = std::thread::Builder::new()
        .name("hotkey-signal".into())
        .spawn(move || listener_thread(app));
    if let Err(e) = spawned {
        log::error!("hotkey signal: listener thread failed to spawn: {e}");
    }
}

fn listener_thread(app: AppHandle) {
    SIGNAL_APP.with(|s| *s.borrow_mut() = Some(app));
    // SAFETY: a standard class registration and message-only window, with a
    // message loop that runs for the life of the process.
    unsafe {
        let instance = match GetModuleHandleW(None) {
            Ok(h) => h,
            Err(e) => {
                log::error!("hotkey signal: GetModuleHandle failed: {e}");
                return;
            }
        };
        let class = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance.into(),
            lpszClassName: w!("YSpot.Signal"),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            log::error!("hotkey signal: RegisterClass failed");
            return;
        }
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("YSpot.Signal"),
            PCWSTR::null(),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance.into()),
            None,
        );
        let Ok(_hwnd) = hwnd else {
            log::error!("hotkey signal: message-only window creation failed");
            return;
        };

        // Registered before the loop starts: a message arriving before we know
        // its id would be dispatched to DefWindowProc and lost, and the sender
        // has no way to learn that happened.
        let message = RegisterWindowMessageW(w!("YKeysSignal"));
        if message == 0 {
            log::error!("hotkey signal: RegisterWindowMessage({MESSAGE_NAME}) failed");
            return;
        }
        SIGNAL_MESSAGE.with(|m| m.set(message));

        log::info!(
            "hotkey signal: listening as class {WINDOW_CLASS} for {MESSAGE_NAME} (§5.1 amended)"
        );
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let registered = SIGNAL_MESSAGE.with(|m| m.get());
    if registered != 0 && msg == registered {
        // Cloned out of the cell before dispatch: the handle goes to another
        // thread below, and holding a `RefCell` borrow across that is a borrow
        // held for longer than it needs to be.
        let app = SIGNAL_APP.with(|s| s.borrow().clone());
        if let Some(app) = app {
            dispatch(&app, wparam.0);
        }
        return LRESULT(0);
    }
    // SAFETY: the documented default handler.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Run the signalled verb on the main thread.
///
/// On the main thread specifically, so that a signal and a chord the shell
/// registered itself take *the same* path: `show` moves and resizes the
/// launcher window and then foregrounds it, and having two threads able to do
/// that concurrently is a race no one would go looking for.
fn dispatch(app: &AppHandle, code: usize) {
    let handle = app.clone();
    let run = app.run_on_main_thread(move || match code {
        code::TOGGLE => crate::toggle(&handle),
        code::SHOW => crate::show(&handle),
        code::SETTINGS => {
            if let Err(e) = crate::show_settings(&handle) {
                log::error!("hotkey signal: could not open Settings: {e}");
            }
        }
        code::CLIPBOARD => {
            if let Err(e) = crate::show_view(&handle, "view:clipboard") {
                log::error!("hotkey signal: could not open the clipboard view: {e}");
            }
        }
        // Named rather than ignored: an unknown code means the config asks for
        // something this build does not have, and the user is owed the reason
        // their chord did nothing.
        other => log::warn!(
            "hotkey signal: code {other} is not one this build knows \
             (0 toggle, 1 show, 2 settings, 3 clipboard)"
        ),
    });
    if let Err(e) = run {
        log::warn!("hotkey signal: could not reach the main thread: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_constants_are_the_ones_ykeys_is_configured_with() {
        // These two strings are the whole contract with an external daemon:
        // the class a sender looks for, and the string both sides hand
        // RegisterWindowMessage. A rename here is a silent break in the field —
        // the sender finds no window, or posts a message nobody matches, and
        // nothing on either side can tell that from "the app is not running".
        assert_eq!(WINDOW_CLASS, "YSpot.Signal");
        assert_eq!(MESSAGE_NAME, "YKeysSignal");
    }

    /// The window is created, and the message registered, from `w!()` literals
    /// — which cannot be built from a `const`, so the literals and the published
    /// constants can drift apart without anything failing to compile.
    #[test]
    fn the_literals_actually_used_match_the_published_constants() {
        // SAFETY: both are static NUL-terminated literals from `w!()`.
        let class: Vec<u16> = unsafe { w!("YSpot.Signal").as_wide().to_vec() };
        assert_eq!(class, WINDOW_CLASS.encode_utf16().collect::<Vec<u16>>());
        let message: Vec<u16> = unsafe { w!("YKeysSignal").as_wide().to_vec() };
        assert_eq!(message, MESSAGE_NAME.encode_utf16().collect::<Vec<u16>>());
    }
}
