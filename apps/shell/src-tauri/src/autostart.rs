//! Autostart (SPEC.md §5.4): the per-user `Run` key.
//!
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, value `YSpot` =
//! `"<install>\yspot.exe" --hidden`. Written by the **shell**, never by the
//! installer: a per-user value written by an installer would exist only for
//! the account that ran it (§9.1). It is writable unelevated, it shows up in
//! Task Manager's Startup tab and in Settings → Apps → Startup where the
//! user can turn it off, and it self-heals — a Run entry whose exe is gone
//! does nothing.
//!
//! The value is written from the running executable's own path, so a moved
//! or reinstalled build corrects itself the next time autostart is enabled.
//! The M4 MSIX build must switch to the `windows.startupTask` extension
//! instead, because packaged apps virtualize registry writes (§5.4).

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
};

use crate::com::wide;

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE_NAME: PCWSTR = w!("YSpot");

/// The command line the Run entry holds: the current exe, quoted, plus the
/// `--hidden` flag that starts the shell warm and invisible (§5.4).
fn run_command() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    Ok(format!("\"{}\" --hidden", exe.display()))
}

struct Key(HKEY);

impl Key {
    fn open(access: windows::Win32::System::Registry::REG_SAM_FLAGS) -> Result<Key, String> {
        let mut key = HKEY::default();
        // SAFETY: static key path; `key` is a valid out-parameter.
        let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, Some(0), access, &mut key) };
        if rc.is_err() {
            return Err(format!("open Run key: {rc:?}"));
        }
        Ok(Key(key))
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: opened above, closed exactly once.
        unsafe {
            let _ = RegCloseKey(self.0);
        };
    }
}

/// Whether the Run value exists (whatever it points at).
pub fn is_enabled() -> bool {
    let Ok(key) = Key::open(KEY_QUERY_VALUE) else {
        return false;
    };
    // SAFETY: valid key; every optional out-parameter is null, which the API
    // documents as "existence check only".
    let rc = unsafe { RegQueryValueExW(key.0, VALUE_NAME, None, None, None, None) };
    rc.is_ok()
}

/// Write the Run value for this user, pointing at this executable.
pub fn enable() -> Result<(), String> {
    let cmd = run_command()?;
    let data = wide(&cmd);
    let key = Key::open(KEY_SET_VALUE)?;
    // The byte length INCLUDES the terminating NUL for REG_SZ, which is what
    // `wide` already appends.
    let bytes: &[u8] = unsafe {
        std::slice::from_raw_parts(data.as_ptr() as *const u8, std::mem::size_of_val(&data[..]))
    };
    // SAFETY: valid key; `bytes` is a NUL-terminated UTF-16 string of exactly
    // this many bytes, which is the REG_SZ contract.
    let rc = unsafe { RegSetValueExW(key.0, VALUE_NAME, None, REG_SZ, Some(bytes)) };
    if rc.is_err() {
        return Err(format!("write Run value: {rc:?}"));
    }
    log::info!("autostart enabled: {cmd}");
    Ok(())
}

/// Remove this user's Run value. Absent is success — the caller asked for
/// "not autostarting", and it is not.
pub fn disable() -> Result<(), String> {
    let key = Key::open(KEY_SET_VALUE)?;
    // SAFETY: valid key and a static value name.
    let rc = unsafe { RegDeleteValueW(key.0, VALUE_NAME) };
    if rc.is_err() && rc.0 != ERROR_FILE_NOT_FOUND.0 {
        return Err(format!("delete Run value: {rc:?}"));
    }
    log::info!("autostart disabled");
    Ok(())
}

/// Whether this process was started by the Run entry (§5.4 warm start).
pub fn started_hidden() -> bool {
    std::env::args().any(|a| a == "--hidden")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes to the real HKCU Run key, so it is `#[ignore]`d: a developer's
    /// `cargo test` must not change whether their machine starts YSpot at
    /// logon. CI runs it with `--ignored`.
    #[test]
    #[ignore = "writes the real HKCU Run key; CI runs it with --ignored"]
    fn enable_then_disable_round_trips() {
        let was = is_enabled();
        enable().expect("enable");
        assert!(is_enabled(), "Run value missing after enable");
        disable().expect("disable");
        assert!(!is_enabled(), "Run value still present after disable");
        // Disabling twice is not an error.
        disable().expect("second disable");
        // Leave the machine as we found it.
        if was {
            enable().expect("restore");
        }
    }

    #[test]
    fn the_run_command_quotes_the_exe_and_carries_the_hidden_flag() {
        let cmd = run_command().expect("current exe");
        assert!(cmd.starts_with('"'), "{cmd}");
        assert!(cmd.ends_with("\" --hidden"), "{cmd}");
        // The path between the quotes is the running test binary.
        let exe = std::env::current_exe().unwrap();
        assert!(cmd.contains(&exe.display().to_string()));
    }
}
