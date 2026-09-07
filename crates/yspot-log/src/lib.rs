//! SPEC §8.5's structured log, shared by every YSpot process.
//!
//! §8.5 fixes one format and one rotation policy for all of them: one JSON
//! object per line (timestamp, level, process, component, message), size
//! rotation, five files of ten megabytes, default level Info. Only the
//! directory differs, and it follows privilege — the per-user processes write
//! `%LOCALAPPDATA%\YSpot\logs`, the elevated service `%ProgramData%\YSpot\logs`.
//!
//! It lives in its own crate because a log a reader has to parse two ways is
//! most of the way to no log at all. The shell had this implementation and
//! the service had none; sharing it is what makes "read the logs" a single
//! instruction during the dogfood instead of two, and keeps the two from
//! drifting the first time either one is touched.
//!
//! Writing to a file rather than a console is not cosmetic for either
//! process. A GUI shell has no console at all. A service does have one in the
//! dev mode M1 ships, but a console is a buffer that scrolls, closes with its
//! window, and is gone by morning — which is exactly when a fortnight of
//! unattended running produces the failure worth reading about.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use log::{Level, LevelFilter, Log, Metadata, Record};

/// §8.5: five files of ten megabytes, per process.
const MAX_BYTES: u64 = 10 * 1024 * 1024;
const MAX_FILES: usize = 5;

/// A `log::Log` that writes §8.5's line format to a rotating file, and also
/// to stderr so a terminal run still shows everything.
struct FileLogger {
    process: &'static str,
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
        let line = format_line(self.process, record);
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
fn format_line(process: &str, record: &Record) -> String {
    let obj = serde_json::json!({
        "ts": timestamp(),
        "level": level_name(record.level()),
        "process": process,
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

/// The token that marks the first line of a run. Stable, because splitting a
/// fortnight of log into runs is the thing a reader does before anything else.
pub const RUN_START: &str = "run-start";

/// Install this process's logger, and write the line that opens the run.
///
/// `process` is §8.5's `process` field — the name a reader greps for when the
/// two logs are read side by side. `path` is the full path of the live log
/// file; `None`, or a path that cannot be opened, falls back to stderr alone,
/// because losing logs is bad and refusing to start is worse.
///
/// The banner is written here rather than left to callers so that no process
/// can forget it. Without one, a log spanning two weeks of logons, crashes and
/// rebuilds is a single undifferentiated stream: nothing says where one run
/// ended and the next began, and §8.5's own crash-free-SESSION metric — one
/// shell process lifetime — cannot be counted from it at all. It carries the
/// resolved path too, which answers "where are my logs" from inside the log,
/// and says so plainly when there is no file and stderr is all there is.
///
/// The level comes from `RUST_LOG`, defaulting to §8.5's Info. Calling this
/// twice is a no-op: `log` accepts one logger per process.
pub fn init(process: &'static str, version: &str, path: Option<PathBuf>) {
    let level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|v| v.parse::<LevelFilter>().ok())
        .unwrap_or(LevelFilter::Info);
    let mut where_to = None;
    let file = path.and_then(|p| match Rotating::open(p.clone()) {
        Ok(r) => {
            where_to = Some(p);
            Some(r)
        }
        Err(e) => {
            eprintln!("log: cannot open {} ({e}); stderr only", p.display());
            None
        }
    });
    let logger = Box::new(FileLogger {
        process,
        file: Mutex::new(file),
        stderr: true,
    });
    if log::set_boxed_logger(logger).is_ok() {
        log::set_max_level(level);
    }
    log::info!(
        "{RUN_START}: {process} {version}, pid {}, {}",
        std::process::id(),
        match &where_to {
            Some(p) => format!("log {}", p.display()),
            None => "no log file; stderr only".to_string(),
        }
    );
}

/// Route panics into the log, so a crash leaves a line in the file.
///
/// std's default hook writes the panic message to stderr and nowhere else.
/// For a process whose stderr is a console that scrolls away — or, for a GUI
/// process, is not connected to anything at all — that means the single most
/// informative line the process ever produces is the one line the log does not
/// have. The last entry in the file is whatever happened just before, and the
/// crash itself leaves no trace.
///
/// The thread name is included because it is usually the whole diagnosis: a
/// panic on `usn-tail` and a panic on a session thread have very different
/// consequences, and only the name distinguishes them.
///
/// Chains to the previous hook rather than replacing it, so a caller that has
/// its own (the shell writes a minidump) keeps it.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("<non-string panic payload>");
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>").to_string();
        match info.location() {
            Some(l) => log::error!(
                "PANIC on thread '{name}' at {}:{}:{}: {payload}",
                l.file(),
                l.line(),
                l.column()
            ),
            None => log::error!("PANIC on thread '{name}' at an unknown location: {payload}"),
        }
        log::logger().flush();
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_line_format_is_one_json_object_per_line() {
        // `format_args!` borrows temporaries that live only to the end of
        // the enclosing statement, so the record is built and used in one.
        let line = format_line(
            "shell",
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

    /// The whole point of sharing this crate: a reader filtering the two logs
    /// by `process` gets the name the process was started with, and nothing
    /// else about the line changes between them.
    #[test]
    fn the_process_field_is_the_only_difference_between_processes() {
        let of = |p| {
            format_line(
                p,
                &Record::builder()
                    .args(format_args!("same message"))
                    .level(Level::Info)
                    .target("same::component")
                    .build(),
            )
        };
        let shell: serde_json::Value = serde_json::from_str(of("shell").trim()).unwrap();
        let indexd: serde_json::Value = serde_json::from_str(of("indexd").trim()).unwrap();
        assert_eq!(shell["process"], "shell");
        assert_eq!(indexd["process"], "indexd");
        for key in ["level", "component", "message"] {
            assert_eq!(shell[key], indexd[key], "`{key}` drifted between processes");
        }
    }

    #[test]
    fn a_message_with_newlines_or_quotes_stays_one_json_line() {
        let line = format_line(
            "shell",
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

    /// The service names its file after the process too, so the rotation
    /// suffix has to follow the stem rather than a hardcoded "shell".
    #[test]
    fn rotation_follows_whatever_the_file_is_called() {
        let dir = std::env::temp_dir().join(format!("yspot-logs-{}-idx", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("indexd.log");
        let mut r = Rotating::open(path.clone()).unwrap();
        r.written = MAX_BYTES;
        r.write_line("{\"a\":1}\n");
        assert!(path.exists());
        assert!(
            dir.join("indexd.1.log").exists(),
            "rotated to the wrong name"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
