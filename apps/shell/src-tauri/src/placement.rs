//! Launcher placement (SPEC.md §5.3): cursor monitor's work area,
//! horizontally centered, top edge at 20% of work-area height, 680×480
//! logical px scaled by the monitor's effective DPI, width clamped to 90%
//! of the work-area width. Recomputed on every show — no persistent state.

use windows_sys::Win32::Foundation::{HWND, POINT};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;

pub const LOGICAL_WIDTH: i32 = 680;
pub const LOGICAL_HEIGHT: i32 = 480;

/// Physical (device-pixel) placement for the launcher window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// §5.3 chain: `GetCursorPos` → `MonitorFromPoint(MONITOR_DEFAULTTONEAREST)`
/// → `GetMonitorInfoW().rcWork` → [`compute_from`].
pub fn compute_placement() -> Option<Placement> {
    compute_placement_of_height(LOGICAL_HEIGHT)
}

/// The same, for a window of a different logical height — the launcher grows
/// to fit an in-place view such as Settings, keeping its top edge where the
/// user is already looking.
pub fn compute_placement_of_height(logical_height: i32) -> Option<Placement> {
    // SAFETY: plain out-parameter Win32 call; POINT is POD and valid
    // zero-initialized.
    let monitor = unsafe {
        let mut pt: POINT = std::mem::zeroed();
        if GetCursorPos(&mut pt) == 0 {
            return None;
        }
        // MONITOR_DEFAULTTONEAREST never returns null.
        MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST)
    };
    on_monitor(monitor, logical_height)
}

/// The placement for a window that is ALREADY on screen, on the monitor it is
/// already on.
///
/// §5.3 puts the launcher on the CURSOR's monitor, which is right when it is
/// being summoned. It is wrong for a resize: growing to fit an in-place view
/// re-ran the cursor lookup, so moving the mouse to another screen and then
/// opening Settings teleported the launcher after it, mid-interaction, and
/// re-scaled it for the wrong DPI on the way.
pub fn compute_placement_for_window(hwnd: isize, logical_height: i32) -> Option<Placement> {
    if hwnd == 0 {
        return None;
    }
    // SAFETY: MONITOR_DEFAULTTONEAREST never returns null for a live window.
    let monitor = unsafe { MonitorFromWindow(hwnd as HWND, MONITOR_DEFAULTTONEAREST) };
    on_monitor(monitor, logical_height)
}

fn on_monitor(monitor: HMONITOR, logical_height: i32) -> Option<Placement> {
    // SAFETY: plain out-parameter Win32 calls; MONITORINFO is POD and valid
    // zero-initialized; cbSize is set before GetMonitorInfoW.
    unsafe {
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut mi) == 0 {
            return None;
        }
        let mut dpi_x: u32 = 96;
        let mut dpi_y: u32 = 96;
        // S_OK == 0; on failure fall back to 96 DPI (1.0 scale).
        if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) != 0 {
            dpi_x = 96;
        }
        let _ = dpi_y; // X and Y effective DPI are always equal.
        Some(compute_from_height(
            (
                mi.rcWork.left,
                mi.rcWork.top,
                mi.rcWork.right,
                mi.rcWork.bottom,
            ),
            dpi_x,
            logical_height,
        ))
    }
}

/// Pure layout math, separated for unit testing. `work` is
/// `(left, top, right, bottom)` of the monitor work area in physical px, and
/// `logical_height` is how tall the window should be. The top
/// edge stays at §5.3's 20% of the work area whatever the height, so growing
/// the window does not move what the user is reading; the height is clamped
/// so a tall view cannot run off the bottom of the work area.
pub fn compute_from_height(work: (i32, i32, i32, i32), dpi: u32, logical_height: i32) -> Placement {
    let (left, top, right, bottom) = work;
    let work_w = (right - left).max(1);
    let work_h = (bottom - top).max(1);
    let dpi = if dpi == 0 { 96 } else { dpi };
    let scale = |v: i32| -> i32 { ((v as i64 * dpi as i64) / 96) as i32 };

    let mut width = scale(LOGICAL_WIDTH);
    let max_width = (work_w as i64 * 9 / 10) as i32;
    if width > max_width {
        width = max_width;
    }
    let y = top + work_h / 5;
    // Clamp to what is left below the top edge, so a tall view (Settings
    // opening in place) cannot run off the bottom of the work area.
    let mut height = scale(logical_height);
    let max_height = (bottom - y).max(1);
    if height > max_height {
        height = max_height;
    }

    let x = left + (work_w - width) / 2;

    Placement {
        x,
        y,
        width: width.max(1) as u32,
        height: height.max(1) as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The launcher at its default height — what every test but the
    /// grow-in-place one is about.
    fn compute_from(work: (i32, i32, i32, i32), dpi: u32) -> Placement {
        compute_from_height(work, dpi, LOGICAL_HEIGHT)
    }

    #[test]
    fn centered_at_96dpi() {
        let p = compute_from((0, 0, 1920, 1040), 96);
        assert_eq!(p.width, 680);
        assert_eq!(p.height, 480);
        assert_eq!(p.x, (1920 - 680) / 2);
        assert_eq!(p.y, 1040 / 5);
    }

    #[test]
    fn scales_at_144dpi() {
        // 150% scale: 680 → 1020, 480 → 720.
        let p = compute_from((0, 0, 2560, 1400), 144);
        assert_eq!(p.width, 1020);
        assert_eq!(p.height, 720);
        assert_eq!(p.x, (2560 - 1020) / 2);
        assert_eq!(p.y, 1400 / 5);
    }

    #[test]
    fn clamps_to_90_percent_of_narrow_work_area() {
        let p = compute_from((0, 0, 600, 800), 96);
        assert_eq!(p.width, 540); // 90% of 600
        assert_eq!(p.x, 30);
    }

    #[test]
    fn secondary_monitor_negative_origin() {
        // Monitor to the left of the primary, with a 100 px top offset.
        let p = compute_from((-1920, 100, 0, 1180), 96);
        assert_eq!(p.x, -1920 + (1920 - 680) / 2);
        assert_eq!(p.y, 100 + 1080 / 5);
    }

    #[test]
    fn a_taller_view_keeps_the_top_edge_and_clamps_to_the_work_area() {
        let work = (0, 0, 1920, 1040);
        let normal = compute_from(work, 96);
        let tall = compute_from_height(work, 96, 620);
        // The top edge does not move when the window grows in place.
        assert_eq!(tall.y, normal.y);
        assert_eq!(tall.x, normal.x);
        assert_eq!(tall.height, 620);
        // A height that would overrun the work area is clamped to what fits
        // below the top edge, never negative and never off-screen.
        let huge = compute_from_height(work, 96, 5000);
        assert_eq!(huge.y, normal.y);
        assert_eq!(huge.height as i32, 1040 - normal.y);
        assert!((huge.y + huge.height as i32) <= 1040);
    }

    #[test]
    fn zero_dpi_falls_back_to_96() {
        let p = compute_from((0, 0, 1920, 1080), 0);
        assert_eq!(p.width, 680);
        assert_eq!(p.height, 480);
    }
}
