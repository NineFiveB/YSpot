//! Diagnostics: structured logs and crash capture (SPEC.md §8.5).
//!
//! A GUI process has no console, so until now the shell's log went nowhere
//! the moment it was not launched from a terminal — which is every real run.
//! §8.5 fixes the location and the format: `%LOCALAPPDATA%\YSpot\logs`, one
//! JSON object per line (timestamp, level, process, component, message),
//! rotated by size, five files of ten megabytes.
//!
//! Crash capture is WER LocalDumps for this per-user executable, which §8.5
//! allows as the alternative to an in-process handler and which costs
//! nothing at runtime: Windows writes the dump, we only say where. The
//! registry value is written **only with consent** and removed the moment
//! consent is withdrawn, so "no consent ⇒ dumps stay local" is enforced by
//! there being no dump at all rather than by a promise not to send it.
//!
//! Upload is **not implemented**: there is no endpoint to upload to, and
//! choosing one is a product decision rather than an implementation detail.
//! Everything that would feed it — consent, local dumps, rotation — is here.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use log::{Level, LevelFilter, Log, Metadata, Record};

/// §8.5: five files of ten megabytes, per process.
const MAX_BYTES: u64 = 10 * 1024 * 1024;
const MAX_FILES: usize = 5;
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

/// A `log::Log` that writes §8.5's line format to a rotating file, and also
/// to stderr so a terminal run still shows everything.
struct FileLogger {
    file: Mutex<Option<Rotating>>,
    stderr: bool,
}

struct Rotating {
    path: PathBuf,
    file: File,
    written: u64,
}

impl Rotating {
    fn open(path: PathBuf) -> std::io::Result<Rotating> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Rotating {
            path,
            file,
            written,
        })
    }

    /// `shell.log` → `shell.1.log` → … → `shell.4.log`, oldest dropped.
    fn rotate(&mut self) -> std::io::Result<()> {
        let stem = self
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("log")
            .to_string();
        let dir = self.path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let nth = |i: usize| dir.join(format!("{stem}.{i}.log"));
        let _ = std::fs::remove_file(nth(MAX_FILES - 1));
        for i in (1..MAX_FILES - 1).rev() {
            let _ = std::fs::rename(nth(i), nth(i + 1));
        }
        let _ = std::fs::rename(&self.path, nth(1));
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.written = 0;
        Ok(())
    }

    fn write_line(&mut self, line: &str) {
        if self.written >= MAX_BYTES && self.rotate().is_err() {
            return;
        }
        if self.file.write_all(line.as_bytes()).is_ok() {
            self.written += line.len() as u64;
        }
    }
}

impl Log for FileLogger {
    fn enabled(&self, _: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format_line(record);
        if self.stderr {
            // Best effort: a closed stderr must not take the process down.
            let _ = std::io::stderr().write_all(line.as_bytes());
        }
        if let Some(f) = self.file.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            f.write_line(&line);
        }
    }

    fn flush(&self) {
        if let Some(f) = self.file.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            let _ = f.file.flush();
        }
    }
}

/// §8.5's line format: one JSON object per line.
fn format_line(record: &Record) -> String {
    let obj = serde_json::json!({
        "ts": timestamp(),
        "level": level_name(record.level()),
        "process": "shell",
        "component": record.target(),
        "message": record.args().to_string(),
    });
    format!("{obj}\n")
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::Error => "error",
        Level::Warn => "warn",
        Level::Info => "info",
        Level::Debug => "debug",
        Level::Trace => "trace",
    }
}

/// RFC 3339 in UTC, without pulling in a date library for one line.
fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let millis = now.subsec_millis();
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

/// Howard Hinnant's `civil_from_days`; the calculator carries the same
/// arithmetic for date math, and neither is worth a dependency.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Install the logger. Falls back to stderr alone if the log directory
/// cannot be opened — losing logs is bad, refusing to start is worse.
pub fn init() {
    let level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|v| v.parse::<LevelFilter>().ok())
        .unwrap_or(LevelFilter::Info);
    let file =
        logs_dir()
            .map(|d| d.join("shell.log"))
            .and_then(|p| match Rotating::open(p.clone()) {
                Ok(r) => Some(r),
                Err(e) => {
                    eprintln!(
                        "diagnostics: cannot open {} ({e}); stderr only",
                        p.display()
                    );
                    None
                }
            });
    let logger = Box::new(FileLogger {
        file: Mutex::new(file),
        stderr: true,
    });
    if log::set_boxed_logger(logger).is_ok() {
        log::set_max_level(level);
    }
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
    let Some(path) = DUMP_PATH.get() else {
        return EXCEPTION_CONTINUE_SEARCH;
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
            return EXCEPTION_CONTINUE_SEARCH;
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
                Some(&exc),
                None,
                None,
            );
            let _ = windows::Win32::Foundation::CloseHandle(HANDLE(file.0));
        }
    }
    // Not EXCEPTION_EXECUTE_HANDLER: swallowing it would leave the process
    // limping instead of dying, and WER would never see the crash.
    EXCEPTION_CONTINUE_SEARCH
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

    #[test]
    fn the_line_format_is_one_json_object_per_line() {
        // `format_args!` borrows temporaries that live only to the end of
        // the enclosing statement, so the record is built and used in one.
        let line = format_line(
            &Record::builder()
                .args(format_args!("hello {}", "world"))
                .level(Level::Warn)
                .target("yspot_shell::apps")
                .build(),
        );
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1, "one line per record");
        let v: serde_json::Value = serde_json::from_str(line.trim()).expect("valid JSON");
        assert_eq!(v["level"], "warn");
        assert_eq!(v["process"], "shell");
        assert_eq!(v["component"], "yspot_shell::apps");
        assert_eq!(v["message"], "hello world");
        let ts = v["ts"].as_str().unwrap();
        assert!(
            ts.ends_with('Z') && ts.len() == 24,
            "unexpected timestamp {ts}"
        );
    }

    #[test]
    fn a_message_with_newlines_or_quotes_stays_one_json_line() {
        let line = format_line(
            &Record::builder()
                .args(format_args!("a \"quoted\" thing\nwith a newline"))
                .level(Level::Error)
                .target("t")
                .build(),
        );
        assert_eq!(
            line.matches('\n').count(),
            1,
            "embedded newline broke the line"
        );
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["message"], "a \"quoted\" thing\nwith a newline");
    }

    #[test]
    fn the_timestamp_is_a_real_date() {
        // 2026-09-03 is 20699 days after the epoch.
        assert_eq!(civil_from_days(20_699), (2026, 9, 3));
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        // A leap day round-trips.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
    }

    #[test]
    fn rotation_shifts_files_and_drops_the_oldest() {
        let dir = std::env::temp_dir().join(format!("yspot-logs-{}-{}", std::process::id(), "rot"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shell.log");
        let mut r = Rotating::open(path.clone()).unwrap();
        // Fill past the threshold four times over.
        for round in 0..4 {
            r.written = MAX_BYTES; // force the next write to rotate
            r.write_line(&format!("{{\"round\":{round}}}\n"));
        }
        assert!(path.exists(), "the live log is gone");
        assert!(dir.join("shell.1.log").exists());
        assert!(dir.join("shell.2.log").exists());
        // Never more than MAX_FILES total.
        let count = std::fs::read_dir(&dir).unwrap().count();
        assert!(count <= MAX_FILES, "{count} files, cap is {MAX_FILES}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Writes to the real HKCU hive, so it is `#[ignore]`d — a developer's
    /// `cargo test` must not change whether Windows dumps their processes.
    #[test]
    #[ignore = "writes the real HKCU WER LocalDumps key; CI runs it with --ignored"]
    fn crash_capture_registers_and_unregisters() {
        set_crash_capture(true).expect("enable");
        set_crash_capture(false).expect("disable");
        // Disabling twice is not an error: absent is the state we wanted.
        set_crash_capture(false).expect("disable again");
    }
}
