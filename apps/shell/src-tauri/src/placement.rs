//! Launcher placement (SPEC.md §5.3): cursor monitor's work area,
//! horizontally centered, top edge at 20% of work-area height, 680×480
//! logical px scaled by the monitor's effective DPI, width clamped to 90%
//! of the work-area width. Recomputed on every show — no persistent state.

use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
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
    // SAFETY: plain out-parameter Win32 calls; POINT/MONITORINFO are POD and
    // valid zero-initialized; cbSize is set before GetMonitorInfoW.
    unsafe {
        let mut pt: POINT = std::mem::zeroed();
        if GetCursorPos(&mut pt) == 0 {
            return None;
        }
        // MONITOR_DEFAULTTONEAREST never returns null.
        let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
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
        Some(compute_from(
            (
                mi.rcWork.left,
                mi.rcWork.top,
                mi.rcWork.right,
                mi.rcWork.bottom,
            ),
            dpi_x,
        ))
    }
}

/// Pure layout math, separated for unit testing. `work` is
/// `(left, top, right, bottom)` of the monitor work area in physical px.
pub fn compute_from(work: (i32, i32, i32, i32), dpi: u32) -> Placement {
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
    let height = scale(LOGICAL_HEIGHT);

    let x = left + (work_w - width) / 2;
    let y = top + work_h / 5;

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
    fn zero_dpi_falls_back_to_96() {
        let p = compute_from((0, 0, 1920, 1080), 0);
        assert_eq!(p.width, 680);
        assert_eq!(p.height, 480);
    }
}
