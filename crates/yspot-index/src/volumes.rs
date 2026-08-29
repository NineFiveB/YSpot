//! Volume discovery and classification — SPEC.md §3.1.
//!
//! Enumerates every volume on the machine via `FindFirstVolumeW`/
//! `FindNextVolumeW`, resolves its mount points, file-system name, and drive
//! type, and flags NTFS fixed volumes (the ones that get the full custom
//! index; everything else falls through to the shell-side passthrough path).
//!
//! Contract: never panic on API failure — a volume that cannot be probed is
//! skipped with a `log::warn!`.

use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_MORE_DATA, ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, GetDriveTypeW, GetVolumeInformationW,
    GetVolumePathNamesForVolumeNameW,
};
use windows_sys::Win32::System::WindowsProgramming::DRIVE_FIXED;

/// One discovered volume (§3.1).
#[derive(Debug, Clone)]
pub struct VolumeInfo {
    /// Volume GUID path as returned by `FindFirstVolumeW`, e.g.
    /// `\\?\Volume{xxxxxxxx-...}\` (trailing backslash included).
    pub guid_path: String,
    /// Mount points (drive roots and mounted-folder paths), e.g. `["C:\\"]`.
    /// May be empty for volumes that are online but not mounted anywhere.
    pub mounts: Vec<String>,
    /// File-system name from `GetVolumeInformationW`, e.g. `NTFS`, `exFAT`.
    pub fs: String,
    /// Raw `GetDriveTypeW` result (`DRIVE_FIXED` = 3, `DRIVE_REMOVABLE` = 2, ...).
    pub drive_type: u32,
    /// `drive_type == DRIVE_FIXED && fs == "NTFS"` — full-index candidates.
    pub is_fixed_ntfs: bool,
}

/// Enumerate all volumes. API failures on an individual volume skip that
/// volume with a warning; a failure to even start enumeration returns an
/// empty list (also warned). Never panics.
pub fn discover() -> Vec<VolumeInfo> {
    let mut out = Vec::new();
    let mut name_buf = [0u16; 512];

    // SAFETY: `name_buf` is valid for 512 u16s for the duration of the call.
    let find = unsafe { FindFirstVolumeW(name_buf.as_mut_ptr(), name_buf.len() as u32) };
    if find == INVALID_HANDLE_VALUE {
        // SAFETY: trivially safe TLS read.
        let err = unsafe { GetLastError() };
        log::warn!("FindFirstVolumeW failed: Win32 error {err}");
        return out;
    }

    loop {
        let guid_path = wide_to_string_nul(&name_buf);
        if let Some(info) = probe_volume(&guid_path) {
            out.push(info);
        }

        // SAFETY: `find` is a live volume-find handle; buffer valid as above.
        let ok = unsafe { FindNextVolumeW(find, name_buf.as_mut_ptr(), name_buf.len() as u32) };
        if ok == 0 {
            // SAFETY: trivially safe TLS read.
            let err = unsafe { GetLastError() };
            if err != ERROR_NO_MORE_FILES {
                log::warn!("FindNextVolumeW failed: Win32 error {err}");
            }
            break;
        }
    }

    // SAFETY: `find` is a live volume-find handle, closed exactly once here.
    unsafe { FindVolumeClose(find) };
    out
}

/// Probe one volume by GUID path; `None` (with a warning) on API failure.
fn probe_volume(guid_path: &str) -> Option<VolumeInfo> {
    let guid_wide = to_utf16z(guid_path);

    let mounts = match volume_mounts(&guid_wide) {
        Ok(m) => m,
        Err(err) => {
            log::warn!(
                "GetVolumePathNamesForVolumeNameW({guid_path}) failed: Win32 error {err}; skipping volume"
            );
            return None;
        }
    };

    let fs = match volume_fs_name(&guid_wide) {
        Ok(f) => f,
        Err(err) => {
            // ERROR_NOT_READY (21) is normal for empty card readers / optical drives.
            log::warn!(
                "GetVolumeInformationW({guid_path}) failed: Win32 error {err}; skipping volume"
            );
            return None;
        }
    };

    // SAFETY: `guid_wide` is NUL-terminated and outlives the call.
    let drive_type = unsafe { GetDriveTypeW(guid_wide.as_ptr()) };
    let is_fixed_ntfs = drive_type == DRIVE_FIXED && fs.eq_ignore_ascii_case("NTFS");

    Some(VolumeInfo {
        guid_path: guid_path.to_string(),
        mounts,
        fs,
        drive_type,
        is_fixed_ntfs,
    })
}

/// Mount points for a volume GUID path, via the double-NUL-terminated list
/// filled by `GetVolumePathNamesForVolumeNameW`. `Err` carries the Win32 code.
fn volume_mounts(guid_wide: &[u16]) -> Result<Vec<String>, u32> {
    let mut buf: Vec<u16> = vec![0; 256];
    // Two attempts: initial buffer, then exact-size retry on ERROR_MORE_DATA.
    for _ in 0..2 {
        let mut ret_len: u32 = 0;
        // SAFETY: `buf` valid for `buf.len()` u16s; `ret_len` is a valid out ptr;
        // `guid_wide` is NUL-terminated.
        let ok = unsafe {
            GetVolumePathNamesForVolumeNameW(
                guid_wide.as_ptr(),
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut ret_len,
            )
        };
        if ok != 0 {
            let filled = (ret_len as usize).min(buf.len());
            return Ok(parse_double_nul_list(&buf[..filled]));
        }
        // SAFETY: trivially safe TLS read.
        let err = unsafe { GetLastError() };
        if err == ERROR_MORE_DATA {
            buf = vec![0; (ret_len as usize).max(256)];
            continue;
        }
        return Err(err);
    }
    Err(ERROR_MORE_DATA)
}

/// File-system name (e.g. `NTFS`) for a volume root path.
fn volume_fs_name(guid_wide: &[u16]) -> Result<String, u32> {
    // MAX_PATH + 1, per API docs.
    let mut fs_buf = [0u16; 261];
    // SAFETY: root path is NUL-terminated; unwanted out params are documented
    // as optional and passed as null; `fs_buf` is valid for its stated length.
    let ok = unsafe {
        GetVolumeInformationW(
            guid_wide.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs_buf.as_mut_ptr(),
            fs_buf.len() as u32,
        )
    };
    if ok == 0 {
        // SAFETY: trivially safe TLS read.
        return Err(unsafe { GetLastError() });
    }
    Ok(wide_to_string_nul(&fs_buf))
}

/// Encode a &str as a NUL-terminated UTF-16 buffer for Win32 `*W` calls.
pub(crate) fn to_utf16z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Decode a wide buffer up to its first NUL (or the whole buffer if none).
fn wide_to_string_nul(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Split a double-NUL-terminated wide-string list into owned strings.
fn parse_double_nul_list(buf: &[u16]) -> Vec<String> {
    buf.split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn double_nul_list_parses_multiple_mounts() {
        // "C:\<NUL>D:\Mount\<NUL><NUL>"
        let mut buf = w("C:\\");
        buf.push(0);
        buf.extend(w("D:\\Mount\\"));
        buf.push(0);
        buf.push(0);
        assert_eq!(parse_double_nul_list(&buf), vec!["C:\\", "D:\\Mount\\"]);
    }

    #[test]
    fn double_nul_list_empty() {
        assert_eq!(parse_double_nul_list(&[0]), Vec::<String>::new());
        assert_eq!(parse_double_nul_list(&[]), Vec::<String>::new());
    }

    #[test]
    fn wide_string_stops_at_nul() {
        let mut buf = w("NTFS");
        buf.push(0);
        buf.extend(w("garbage"));
        assert_eq!(wide_to_string_nul(&buf), "NTFS");
        // No NUL at all: whole buffer.
        assert_eq!(wide_to_string_nul(&w("exFAT")), "exFAT");
    }

    #[test]
    fn utf16z_is_nul_terminated() {
        let z = to_utf16z("C:");
        assert_eq!(z, vec![b'C' as u16, b':' as u16, 0]);
    }
}
