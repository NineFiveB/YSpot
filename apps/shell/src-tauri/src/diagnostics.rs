//! Diagnostics: structured logs and crash capture (SPEC.md §8.5).
//!
//! A GUI process has no console, so until now the shell's log went nowhere
//! the moment it was not launched from a terminal — which is every real run.
//! §8.5 fixes the location and the format: `%LOCALAPPDATA%\YSpot\logs`, one
//! JSON object per line (timestamp, level, process, component, message),
//! rotated by size, five files of ten megabytes. The writer itself is
//! [`yspot_log`], shared with the service — §8.5 specifies one format for
//! every process, and two implementations of it would only stay identical
//! until the first one was touched.
//!
//! Crash capture is the **in-process** minidump handler of §8.5's first
//! option: a top-level exception filter, plus a panic hook for the crashes
//! SEH never sees, both writing the dump themselves. It began as WER
//! LocalDumps under HKCU — the alternative §8.5 also allows — and moved
//! because a registry key only catches what Windows dispatches as an
//! exception, and the crash this shell actually dies from is a Rust panic
//! unwinding out of an `extern "system"` boundary, which `__fastfail`s past
//! every filter. `remove_stale_localdumps` tidies away the key that version
//! left behind.
//!
//! Consent gates it at the source: `CAPTURE_ON` is set from the stored
//! setting and read by `write_dump`, so "no consent ⇒ dumps stay local" holds
//! because no dump is written at all, rather than by a promise not to send
//! one.
//!
//! Upload is **not implemented**: there is no endpoint to upload to, and
//! choosing one is a product decision rather than an implementation detail.
//! Everything that would feed it — consent, local dumps, rotation — is here.

use std::path::PathBuf;
/// Crash dumps kept when consent is on; older ones are rotated away.
const MAX_DUMPS: usize = 10;

/// The per-user root, `%LOCALAPPDATA%\YSpot`.
pub fn data_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|b| PathBuf::from(b).join("YSpot"))
}

pub fn logs_dir() -> Option<PathBuf> {
    data_dir().map(|d| d.join("logs"))
}

pub fn crashes_dir() -> Option<PathBuf> {
    data_dir().map(|d| d.join("crashes"))
}

/// Install the logger.
///
/// The implementation is [`yspot_log`], shared with the service so both
/// processes write §8.5's one format. The `"shell"` passed here becomes the
/// line's `process` field, which is what tells the two apart when the logs
/// are read as one timeline.
pub fn init() {
    yspot_log::init(
        "shell",
        env!("CARGO_PKG_VERSION"),
        logs_dir().map(|d| d.join("shell.log")),
    );
}

// ---------------------------------------------------------------------------
// Crash capture (§8.5)

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_WRITE, FILE_SHARE_NONE,
};
use windows::Win32::System::Diagnostics::Debug::{
    MiniDumpWithIndirectlyReferencedMemory, MiniDumpWithThreadInfo, MiniDumpWriteDump,
    SetUnhandledExceptionFilter, EXCEPTION_POINTERS, MINIDUMP_EXCEPTION_INFORMATION,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId,
};

/// Whether consent is currently given. Read by the crash handler, so it is an
/// atomic and nothing else.
static CAPTURE_ON: AtomicBool = AtomicBool::new(false);

/// Set once a dump has been written, so a second thread arriving in the
/// handler (or a panic following a fault) does not truncate the first.
static DUMP_WRITTEN: AtomicBool = AtomicBool::new(false);

/// The full path this process would write its dump to, NUL-terminated UTF-16,
/// computed once when capture is first enabled.
///
/// Precomputed on purpose: the handler runs after the process has already gone
/// wrong, possibly with a corrupt heap or on a thread with almost no stack
/// left, and formatting a path there would be the second bug. One dump per
/// process run is all a crash produces anyway.
static DUMP_PATH: OnceLock<Vec<u16>> = OnceLock::new();

/// §8.5's crash capture, as the in-process minidump handler that section
/// offers as the alternative to WER LocalDumps.
///
/// It has to be in-process. LocalDumps is only ever read from **HKLM**
/// (`HKLM\SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps`),
/// which needs administrator rights, and §2 pins the shell to the interactive
/// user at medium integrity, unelevated — so it cannot write that key, and the
/// per-user hive Windows never consults. An earlier version wrote HKCU and
/// reported success: consent was accepted, the setting showed enabled, and no
/// dump was ever produced by anything. §8.5's service equivalent still goes in
/// HKLM, written by the service MSI, which runs elevated.
pub fn set_crash_capture(enabled: bool) -> Result<(), String> {
    // Tidy away the key the HKCU version left behind. Ignoring the result on
    // purpose: absent is the state we want, and it is not worth a failure.
    remove_stale_localdumps();

    if !enabled {
        CAPTURE_ON.store(false, Ordering::SeqCst);
        log::info!("crash capture disabled (§8.5)");
        return Ok(());
    }

    let dir = crashes_dir().ok_or_else(|| "LOCALAPPDATA unset".to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;

    // SAFETY: a documented process-wide filter; the handler below is a plain
    // `extern "system"` fn with the required signature.
    let path = DUMP_PATH.get_or_init(|| {
        // SAFETY: reading our own ids.
        let pid = unsafe { GetCurrentProcessId() };
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let file = dir.join(format!("yspot-shell-{pid}-{stamp}.dmp"));
        file.to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect()
    });
    let _ = path;
    if !CAPTURE_ON.swap(true, Ordering::SeqCst) {
        // SAFETY: installing a process-wide unhandled-exception filter.
        unsafe { SetUnhandledExceptionFilter(Some(on_unhandled_exception)) };
    }
    log::info!("crash capture enabled; dumps land in {}", dir.display());
    Ok(())
}

/// Remove the HKCU LocalDumps key an earlier build wrote, which Windows never
/// read. Best effort: it is housekeeping, not a feature.
fn remove_stale_localdumps() {
    use windows::core::w;
    use windows::Win32::System::Registry::{RegDeleteTreeW, HKEY_CURRENT_USER};
    // SAFETY: static key path under the user's own hive.
    unsafe {
        let _ = RegDeleteTreeW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\Windows Error Reporting\LocalDumps\yspot-shell.exe"),
        );
    }
}

/// Catch the crash Windows never tells us about.
///
/// [`on_unhandled_exception`] only sees SEH-dispatched exceptions, and a Rust
/// panic that unwinds out of an `extern "system"` boundary is not one: it
/// reaches `panic_cannot_unwind` and `__fastfail`, which goes straight to WER
/// past every handler, the top-level filter included. Measured, not assumed —
/// a probe panicking across such a boundary exits 0xC0000409 with the filter
/// never entered, while a null dereference in the same probe exits 0xC0000005
/// with it entered.
///
/// That is the crash class this shell actually dies from. Every
/// `#[tauri::command]` runs on the WebView2 IPC callback, an `extern "system"`
/// COM vtable entry with no `catch_unwind` between it and our code — which is
/// exactly how `72°f in c` took the launcher down. So the hook is where a
/// panic gets recorded, and it runs BEFORE the abort, on the panicking thread,
/// with the stack intact.
///
/// It matters twice over because a release build is `windows_subsystem =
/// "windows"`: std's own panic message goes to a stderr nobody is connected
/// to, so without this the log's last line is whatever happened before the
/// crash and the crash itself leaves no trace at all.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("<non-string panic payload>");
        match info.location() {
            Some(l) => log::error!(
                "PANIC at {}:{}:{}: {payload}",
                l.file(),
                l.line(),
                l.column()
            ),
            None => log::error!("PANIC at an unknown location: {payload}"),
        }
        // No exception record to hand it: this is a panic, not a fault, so the
        // dump carries the threads and their stacks and nothing else.
        write_dump(std::ptr::null());
        previous(info);
    }));
}

/// Windows' last call before the process dies. Writes one minidump and lets
/// the default handler carry on, so WER still sees the crash and
/// `RegisterApplicationRestart` (§8.5) still relaunches us.
///
/// Everything here is deliberately minimal: no allocation, no logging, no
/// locks. The heap may be the reason we are here.
unsafe extern "system" fn on_unhandled_exception(info: *const EXCEPTION_POINTERS) -> i32 {
    const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
    if !CAPTURE_ON.load(Ordering::Relaxed) {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    write_dump(info);
    // Not EXCEPTION_EXECUTE_HANDLER: swallowing it would leave the process
    // limping instead of dying, and WER would never see the crash.
    EXCEPTION_CONTINUE_SEARCH
}

/// Write this process's one minidump, if capture is on and a path was fixed.
///
/// `info` may be null: a panic has no `EXCEPTION_POINTERS`, and
/// `MiniDumpWriteDump` accepts the absence.
///
/// Deliberately minimal — no allocation, no logging, no locks. On the fault
/// path the heap may be the reason we are here; on the panic path we are
/// about to abort either way.
fn write_dump(info: *const EXCEPTION_POINTERS) {
    if !CAPTURE_ON.load(Ordering::Relaxed) {
        return;
    }
    // One dump per process run. Two threads faulting at once would otherwise
    // write over each other and leave a truncated file that opens in nothing.
    if DUMP_WRITTEN.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(path) = DUMP_PATH.get() else {
        return;
    };
    // SAFETY: `path` is a NUL-terminated UTF-16 buffer that outlives the call;
    // the handles come from the current process.
    unsafe {
        let Ok(file) = CreateFileW(
            PCWSTR(path.as_ptr()),
            FILE_GENERIC_WRITE.0,
            FILE_SHARE_NONE,
            None,
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            None,
        ) else {
            return;
        };
        if file != INVALID_HANDLE_VALUE {
            let exc = MINIDUMP_EXCEPTION_INFORMATION {
                ThreadId: GetCurrentThreadId(),
                ExceptionPointers: info as *mut _,
                ClientPointers: false.into(),
            };
            let _ = MiniDumpWriteDump(
                GetCurrentProcess(),
                GetCurrentProcessId(),
                file,
                MiniDumpWithThreadInfo | MiniDumpWithIndirectlyReferencedMemory,
                (!info.is_null()).then_some(&exc as *const _),
                None,
                None,
            );
            let _ = windows::Win32::Foundation::CloseHandle(HANDLE(file.0));
        }
    }
}

/// capture is on; this also cleans up after consent is withdrawn.
pub fn rotate_dumps() {
    let Some(dir) = crashes_dir() else { return };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut dumps: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("dmp"))
        })
        .filter_map(|e| {
            let t = e.metadata().ok()?.modified().ok()?;
            Some((t, e.path()))
        })
        .collect();
    if dumps.len() <= MAX_DUMPS {
        return;
    }
    dumps.sort_by_key(|(t, _)| *t);
    let excess = dumps.len() - MAX_DUMPS;
    for (_, path) in dumps.into_iter().take(excess) {
        if let Err(e) = std::fs::remove_file(&path) {
            log::debug!("could not rotate away {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Touches this developer's real `%LOCALAPPDATA%\YSpot\crashes` and
    /// installs a process-wide exception filter, so it is `#[ignore]`d; CI
    /// runs it with `--ignored`.
    ///
    /// It used to say it wrote the HKCU WER LocalDumps key. It has not done
    /// that since review finding 38 replaced LocalDumps with the in-process
    /// handler — `set_crash_capture` only REMOVES that key now — and a test
    /// whose stated reason for being ignored is no longer true invites
    /// someone to un-ignore it for the wrong reason.
    #[test]
    #[ignore = "creates the real crashes dir and installs a process-wide filter; CI runs it with --ignored"]
    fn crash_capture_arms_and_disarms_the_handler() {
        set_crash_capture(true).expect("enable");
        // The gate the handler itself reads. Asserting on it is the whole
        // point: before this, the test only proved the calls returned Ok,
        // which they would have done with the body deleted.
        assert!(
            CAPTURE_ON.load(Ordering::SeqCst),
            "capture was enabled but the flag the dump path reads is still off"
        );
        assert!(
            crashes_dir().expect("LOCALAPPDATA").is_dir(),
            "nowhere for a dump to land"
        );

        set_crash_capture(false).expect("disable");
        assert!(
            !CAPTURE_ON.load(Ordering::SeqCst),
            "consent was withdrawn and a crash would still be dumped"
        );

        // Disabling twice is not an error: absent is the state we wanted.
        set_crash_capture(false).expect("disable again");
        assert!(!CAPTURE_ON.load(Ordering::SeqCst));
    }
}
