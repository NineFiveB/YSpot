//! Window management (SPEC.md §7.5): find the open windows, switch to one,
//! and lay it out.
//!
//! Open windows are a search source like apps and files, so switching to the
//! browser tab you were in is the same gesture as launching anything else.
//!
//! Enumeration is cached rather than run per keystroke: `EnumWindows` with a
//! DWM query per window costs a millisecond or two, which is most of the
//! §2.5 shell-routing budget for something that changes when a window opens,
//! not when a key is pressed. The cache refreshes when the launcher is shown
//! and whenever it is older than [`CACHE_TTL`].

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::BOOL;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, MAX_PATH, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW,
    PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_MENU,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetAncestor, GetForegroundWindow, GetLastActivePopup, GetWindowLongPtrW,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    PostMessageW, SetForegroundWindow, SetWindowPos, ShowWindow, GA_ROOTOWNER, GWL_EXSTYLE,
    HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_MINIMIZE, SW_RESTORE,
    WM_CLOSE, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};

use crate::matcher::{self, Ranges, Target};

/// How stale an enumeration may be before a query re-runs it.
const CACHE_TTL: Duration = Duration::from_secs(5);
/// Most window rows a root query shows.
pub const MAX_RESULTS: usize = 5;

#[derive(Clone, Debug)]
pub struct WindowEntry {
    /// The window handle as an integer — the stable row id for this source
    /// (§5.6) for as long as the window lives, which is all a launcher needs.
    pub id: isize,
    pub title: String,
    /// Executable name of the owning process, shown as the row's subtitle.
    pub process: String,
    target: Target,
    process_target: Target,
}

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowMatch {
    pub id: String,
    pub title: String,
    pub process: String,
    pub score: f32,
    pub match_ranges: Ranges,
}

/// Cached enumeration, refreshed on show and on a TTL.
pub struct WindowCache {
    inner: Mutex<(Vec<WindowEntry>, Option<Instant>)>,
}

impl WindowCache {
    pub fn new() -> Arc<WindowCache> {
        Arc::new(WindowCache {
            inner: Mutex::new((Vec::new(), None)),
        })
    }

    /// The current window list, re-enumerating if the cache has expired.
    pub fn snapshot(&self) -> Vec<WindowEntry> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let stale = guard.1.is_none_or(|t| t.elapsed() > CACHE_TTL);
        if stale {
            guard.0 = enumerate();
            guard.1 = Some(Instant::now());
            log::debug!(
                "windows: {} alt-tab-eligible ({})",
                guard.0.len(),
                guard
                    .0
                    .iter()
                    .take(5)
                    .map(|w| format!("{:?}", w.title))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        guard.0.clone()
    }

    /// Drop the cache so the next query re-enumerates — called when the
    /// launcher is shown, since that is when the list is about to be read
    /// and when it is most likely to have changed.
    pub fn invalidate(&self) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).1 = None;
    }
}

/// Alt-tab-eligible top-level windows (§7.5's filter, in order of cost).
pub fn enumerate() -> Vec<WindowEntry> {
    let mut out: Vec<WindowEntry> = Vec::with_capacity(64);
    // SAFETY: the callback below only touches the Vec handed to it through
    // `lparam`, which outlives the call; EnumWindows is synchronous.
    unsafe {
        let _ = EnumWindows(
            Some(enum_proc),
            LPARAM(&mut out as *mut Vec<WindowEntry> as isize),
        );
    }
    out
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: `lparam` is the Vec pointer `enumerate` passed in, valid for
    // the whole enumeration and touched by nothing else.
    let out = unsafe { &mut *(lparam.0 as *mut Vec<WindowEntry>) };
    if let Some(entry) = describe(hwnd) {
        out.push(entry);
    }
    true.into()
}

/// Whether `hwnd` is a window a person would alt-tab to, and what to call it.
fn describe(hwnd: HWND) -> Option<WindowEntry> {
    // SAFETY: every call here takes a window handle the enumeration just
    // produced; all are read-only queries.
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return None;
        }
        // §7.5: the window must be the one its owner chain would surface in
        // alt-tab. This is the documented walk — climb to the root owner,
        // take its last active popup, and keep climbing while that popup is
        // invisible — and the window qualifies only if the walk lands back
        // on it. An approximation here shows duplicate rows for every app
        // with a dialog open.
        let mut walk = HWND(std::ptr::null_mut());
        let mut try_hwnd = GetAncestor(hwnd, GA_ROOTOWNER);
        while try_hwnd != walk {
            walk = try_hwnd;
            try_hwnd = GetLastActivePopup(walk);
            if IsWindowVisible(try_hwnd).as_bool() {
                break;
            }
            try_hwnd = GetAncestor(try_hwnd, GA_ROOTOWNER);
        }
        if try_hwnd != hwnd {
            return None;
        }
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        // A tool window is chrome, unless it also asks to appear in alt-tab.
        if ex & WS_EX_TOOLWINDOW.0 != 0 && ex & WS_EX_APPWINDOW.0 == 0 {
            return None;
        }
        // Cloaked windows are the ghosts of suspended UWP apps; the DWM
        // query is the only way to tell them from real ones.
        let mut cloaked: u32 = 0;
        let _ = DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
        );
        if cloaked != 0 {
            return None;
        }
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return None; // no title: not something to switch to by name
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        if n <= 0 {
            return None;
        }
        let title = String::from_utf16_lossy(&buf[..n as usize]);
        let process = process_name(hwnd).unwrap_or_default();
        Some(WindowEntry {
            id: hwnd.0 as isize,
            target: Target::new(&title),
            process_target: Target::new(&process),
            title,
            process,
        })
    }
}

/// Executable name of the process owning `hwnd`.
fn process_name(hwnd: HWND) -> Option<String> {
    let mut pid = 0u32;
    // SAFETY: valid window handle; `pid` is a valid out-slot.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return None;
    }
    // SAFETY: LIMITED_INFORMATION is the least right that answers this, and
    // the handle is closed on every path below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buf = [0u16; MAX_PATH as usize];
    let mut len = buf.len() as u32;
    // SAFETY: `buf`/`len` describe one buffer; the handle is valid here.
    let ok = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    };
    // SAFETY: opened above, closed exactly once.
    unsafe {
        let _ = CloseHandle(handle);
    };
    ok.ok()?;
    let full = String::from_utf16_lossy(&buf[..len as usize]);
    Some(full.rsplit(['\\', '/']).next().unwrap_or(&full).to_string())
}

/// Match a query against the window list. The title carries the highlight;
/// the process name matches too, so `chrome` finds every browser window.
pub fn match_query(windows: &[WindowEntry], query: &str, max: usize) -> Vec<WindowMatch> {
    let q = matcher::fold_query(query);
    // Two characters: one letter would fill the page with windows.
    if q.len() < 2 || max == 0 {
        return Vec::new();
    }
    let mut hits: Vec<(f32, &WindowEntry, Ranges)> = windows
        .iter()
        .filter_map(|w| {
            matcher::score_with_synonyms(&w.target, std::slice::from_ref(&w.process_target), &q)
                .map(|(s, r)| (s, w, r))
        })
        .collect();
    hits.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.1.title.len().cmp(&b.1.title.len()))
            .then_with(|| a.1.title.cmp(&b.1.title))
    });
    hits.truncate(max);
    hits.into_iter()
        .map(|(score, w, match_ranges)| WindowMatch {
            id: w.id.to_string(),
            title: w.title.clone(),
            process: w.process.clone(),
            score,
            match_ranges,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Actions

/// Where to put a window (§7.5's presets), as a fraction of the work area.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tile {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// The preset named by `action`, or `None` if it is not a layout action.
pub fn tile_for(action: &str) -> Option<Tile> {
    let t = |x, y, w, h| Some(Tile { x, y, w, h });
    match action {
        "left_half" => t(0.0, 0.0, 0.5, 1.0),
        "right_half" => t(0.5, 0.0, 0.5, 1.0),
        "top_half" => t(0.0, 0.0, 1.0, 0.5),
        "bottom_half" => t(0.0, 0.5, 1.0, 0.5),
        "left_third" => t(0.0, 0.0, 1.0 / 3.0, 1.0),
        "center_third" => t(1.0 / 3.0, 0.0, 1.0 / 3.0, 1.0),
        "right_third" => t(2.0 / 3.0, 0.0, 1.0 / 3.0, 1.0),
        "left_two_thirds" => t(0.0, 0.0, 2.0 / 3.0, 1.0),
        "right_two_thirds" => t(1.0 / 3.0, 0.0, 2.0 / 3.0, 1.0),
        "top_left" => t(0.0, 0.0, 0.5, 0.5),
        "top_right" => t(0.5, 0.0, 0.5, 0.5),
        "bottom_left" => t(0.0, 0.5, 0.5, 0.5),
        "bottom_right" => t(0.5, 0.5, 0.5, 0.5),
        "maximize" => t(0.0, 0.0, 1.0, 1.0),
        "center" => t(0.25, 0.125, 0.5, 0.75),
        _ => None,
    }
}

/// The pixel rectangle a tile names inside a work area.
pub fn tile_rect(work: (i32, i32, i32, i32), tile: Tile) -> (i32, i32, i32, i32) {
    let (left, top, right, bottom) = work;
    let w = (right - left) as f32;
    let h = (bottom - top) as f32;
    let x = left + (w * tile.x).round() as i32;
    let y = top + (h * tile.y).round() as i32;
    // At least one pixel each way, so a degenerate tile cannot hide a window.
    let width = ((w * tile.w).round() as i32).max(1);
    let height = ((h * tile.h).round() as i32).max(1);
    (x, y, width, height)
}

fn hwnd_of(id: &str) -> Result<HWND, String> {
    let raw: isize = id.parse().map_err(|_| format!("bad window id {id}"))?;
    Ok(HWND(raw as *mut _))
}

/// Run a §7.5 action on a window.
pub fn act(id: &str, action: &str) -> Result<(), String> {
    let hwnd = hwnd_of(id)?;
    match action {
        "open" | "switch" => switch_to(hwnd),
        "minimize" => {
            // SAFETY: valid window handle; a show-command is a message.
            unsafe {
                let _ = ShowWindow(hwnd, SW_MINIMIZE);
            };
            Ok(())
        }
        "close" => {
            // §7.5: never TerminateProcess. WM_CLOSE lets the app save.
            // SAFETY: valid window handle; posting is asynchronous.
            unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) }
                .map_err(|e| format!("close: {e}"))
        }
        "topmost" | "untopmost" => {
            let after = if action == "topmost" {
                HWND_TOPMOST
            } else {
                HWND_NOTOPMOST
            };
            // SAFETY: valid handles; NOMOVE|NOSIZE means only the Z order
            // changes, so the position arguments are ignored.
            unsafe {
                SetWindowPos(
                    hwnd,
                    Some(after),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                )
            }
            .map_err(|e| format!("topmost: {e}"))
        }
        other => match tile_for(other) {
            Some(tile) => place(hwnd, tile),
            None => Err(format!("unknown window action {other}")),
        },
    }
}

/// Restore and raise, with §7.5's foreground-lock workaround.
///
/// Windows denies `SetForegroundWindow` to a process that is not the last
/// input receiver, flashing the taskbar button instead. The accepted fix is
/// to make this thread the last-input one with a no-op Alt press, with
/// `AttachThreadInput` to the current foreground thread as the fallback.
fn switch_to(hwnd: HWND) -> Result<(), String> {
    // SAFETY: all calls take valid handles/ids; the synthetic Alt is a
    // press/release pair, so no modifier is left stuck down.
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        nudge_alt();
        if SetForegroundWindow(hwnd).as_bool() {
            return Ok(());
        }
        // Fallback: borrow the foreground thread's input queue for the call.
        let foreground = GetForegroundWindow();
        let target_thread = GetWindowThreadProcessId(hwnd, None);
        let fg_thread = GetWindowThreadProcessId(foreground, None);
        let me = GetCurrentThreadId();
        let attached_fg =
            fg_thread != 0 && fg_thread != me && AttachThreadInput(me, fg_thread, true).as_bool();
        let attached_target = target_thread != 0
            && target_thread != me
            && AttachThreadInput(me, target_thread, true).as_bool();
        let ok = SetForegroundWindow(hwnd).as_bool();
        if attached_target {
            let _ = AttachThreadInput(me, target_thread, false);
        }
        if attached_fg {
            let _ = AttachThreadInput(me, fg_thread, false);
        }
        if ok {
            Ok(())
        } else {
            Err("Windows refused to raise the window (foreground lock)".to_string())
        }
    }
}

/// A no-op Alt press/release, purely to make this thread the last input
/// receiver so the foreground change is allowed.
///
/// # Safety
/// Injects input into this session. Only ever called on the path where the
/// user has just chosen a window to switch to.
unsafe fn nudge_alt() {
    let mut input = [INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VK_MENU,
                ..Default::default()
            },
        },
    }; 2];
    input[1].Anonymous.ki.dwFlags = KEYEVENTF_KEYUP;
    // SAFETY: a well-formed two-event array of the size passed alongside it.
    unsafe { SendInput(&input, std::mem::size_of::<INPUT>() as i32) };
}

/// Move and size a window into a tile of its current monitor's work area.
fn place(hwnd: HWND, tile: Tile) -> Result<(), String> {
    // SAFETY: valid window handle; MONITORINFO is POD with cbSize set.
    let work = unsafe {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut mi).as_bool() {
            return Err("could not read the monitor work area".to_string());
        }
        let RECT {
            left,
            top,
            right,
            bottom,
        } = mi.rcWork;
        (left, top, right, bottom)
    };
    let (x, y, w, h) = tile_rect(work, tile);
    // SAFETY: valid window handle; no Z-order change, no activation.
    unsafe { SetWindowPos(hwnd, None, x, y, w, h, SWP_NOACTIVATE) }
        .map_err(|e| format!("move/resize: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiles_cover_the_work_area_and_never_degenerate() {
        let work = (0, 0, 1920, 1080);
        assert_eq!(
            tile_rect(work, tile_for("maximize").unwrap()),
            (0, 0, 1920, 1080)
        );
        assert_eq!(
            tile_rect(work, tile_for("left_half").unwrap()),
            (0, 0, 960, 1080)
        );
        assert_eq!(
            tile_rect(work, tile_for("right_half").unwrap()),
            (960, 0, 960, 1080)
        );
        assert_eq!(
            tile_rect(work, tile_for("top_left").unwrap()),
            (0, 0, 960, 540)
        );
        assert_eq!(
            tile_rect(work, tile_for("bottom_right").unwrap()),
            (960, 540, 960, 540)
        );
        // Thirds meet without a gap or an overlap at this width.
        let (lx, _, lw, _) = tile_rect(work, tile_for("left_third").unwrap());
        let (cx, _, cw, _) = tile_rect(work, tile_for("center_third").unwrap());
        let (rx, _, rw, _) = tile_rect(work, tile_for("right_third").unwrap());
        assert_eq!(lx + lw, cx);
        assert_eq!(cx + cw, rx);
        assert_eq!(rx + rw, 1920);
    }

    #[test]
    fn tiles_respect_a_work_area_that_is_not_at_the_origin() {
        // A secondary monitor to the left, and a taskbar taking 40 px.
        let work = (-1920, 0, 0, 1040);
        assert_eq!(
            tile_rect(work, tile_for("left_half").unwrap()),
            (-1920, 0, 960, 1040)
        );
        assert_eq!(
            tile_rect(work, tile_for("right_half").unwrap()),
            (-960, 0, 960, 1040)
        );
        assert_eq!(
            tile_rect(work, tile_for("maximize").unwrap()),
            (-1920, 0, 1920, 1040)
        );
    }

    #[test]
    fn a_degenerate_work_area_still_yields_a_visible_window() {
        let (_, _, w, h) = tile_rect((0, 0, 1, 1), tile_for("left_third").unwrap());
        assert!(w >= 1 && h >= 1);
    }

    #[test]
    fn unknown_actions_are_not_tiles() {
        assert!(tile_for("open").is_none());
        assert!(tile_for("close").is_none());
        assert!(tile_for("").is_none());
        assert!(tile_for("nonsense").is_none());
    }

    #[test]
    fn a_bad_window_id_is_an_error_not_a_panic() {
        assert!(act("not-a-number", "open").is_err());
        assert!(act("12345", "nonsense").is_err());
    }

    /// The real desktop: enumeration must find this process's own windows
    /// among others, with titles and process names.
    #[test]
    fn enumeration_finds_real_windows() {
        let list = enumerate();
        assert!(!list.is_empty(), "no alt-tab-eligible windows found");
        assert!(list.iter().all(|w| !w.title.is_empty()));
        assert!(
            list.iter().any(|w| !w.process.is_empty()),
            "no process names resolved"
        );
        // Ids are unique per window.
        let mut ids: Vec<isize> = list.iter().map(|w| w.id).collect();
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n, "duplicate window ids");
    }

    #[test]
    fn matching_finds_a_window_by_title_or_by_process() {
        let entries = vec![
            WindowEntry {
                id: 1,
                target: Target::new("Inbox — Outlook"),
                process_target: Target::new("outlook.exe"),
                title: "Inbox — Outlook".into(),
                process: "outlook.exe".into(),
            },
            WindowEntry {
                id: 2,
                target: Target::new("YSpot — main"),
                process_target: Target::new("code.exe"),
                title: "YSpot — main".into(),
                process: "code.exe".into(),
            },
        ];
        let titles = |q: &str| -> Vec<String> {
            match_query(&entries, q, MAX_RESULTS)
                .into_iter()
                .map(|m| m.title)
                .collect()
        };
        assert_eq!(titles("inbox"), vec!["Inbox — Outlook"]);
        // The process name matches too, so `code` finds the editor window.
        assert_eq!(titles("code"), vec!["YSpot — main"]);
        // One character is not a window query.
        assert!(match_query(&entries, "i", MAX_RESULTS).is_empty());
        assert!(match_query(&entries, "zzqxjv", MAX_RESULTS).is_empty());
    }
}
