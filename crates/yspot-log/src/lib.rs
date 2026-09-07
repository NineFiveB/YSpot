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
use std::sync::atomic::{AtomicBool, Ordering};
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

/// After a failed rotation, how many more bytes to accept before trying
/// again. Small enough that a transient block (an antivirus scan, a backup
/// agent holding the file) is recovered from within one busy minute; large
/// enough that a permanent one is not three syscalls on every single line.
const ROTATE_RETRY_BYTES: u64 = 1024 * 1024;

struct Rotating {
    path: PathBuf,
    file: File,
    written: u64,
    /// The size at which to attempt the next rotation. Normally `MAX_BYTES`;
    /// pushed out after a failure so a log that cannot be rotated is not
    /// retried on every line.
    rotate_at: u64,
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
            rotate_at: MAX_BYTES,
        })
    }

    /// `shell.log` → `shell.1.log` → … → `shell.4.log`, oldest dropped.
    ///
    /// Ordered so that **nothing is destroyed until both fallible steps have
    /// succeeded.** The obvious order — drop the oldest, shift the rest, then
    /// rename the live file and reopen it — does the irreversible work first
    /// and the failable work last, which turns any persistent failure into
    /// total loss: a full disk, a directory whose ACL changed, or Controlled
    /// Folder Access blocking the create, and five log lines later the shift
    /// cascade has walked all five generations into the slot it deletes.
    /// Fifty megabytes of the only record of what went wrong, gone in under a
    /// second, silently, at exactly the moment it was needed.
    ///
    /// So: move the live file aside first (fails harmlessly, nothing lost),
    /// then create its replacement (on failure, move it back), and only then
    /// touch the history.
    fn rotate(&mut self) -> std::io::Result<()> {
        let stem = self
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("log")
            .to_string();
        let dir = self.path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let nth = |i: usize| dir.join(format!("{stem}.{i}.log"));

        // A leftover from a process that died mid-rotation. Removing it can
        // fail harmlessly; the rename below is the operation that matters.
        let pending = dir.join(format!("{stem}.rotating"));
        let _ = std::fs::remove_file(&pending);

        // Fallible step 1. A sharing violation — an editor, a backup agent, a
        // tail running during the dogfood — lands here and costs nothing.
        std::fs::rename(&self.path, &pending)?;

        // Fallible step 2. If the replacement cannot be created, put the live
        // file back so the next write still has somewhere to go.
        let replacement = match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(f) => f,
            Err(e) => {
                let _ = std::fs::rename(&pending, &self.path);
                return Err(e);
            }
        };

        // Past this point every step is destructive and none can fail in a way
        // that loses the live log, which is now safely aside.
        let _ = std::fs::remove_file(nth(MAX_FILES - 1));
        for i in (1..MAX_FILES - 1).rev() {
            let _ = std::fs::rename(nth(i), nth(i + 1));
        }
        let _ = std::fs::rename(&pending, nth(1));

        self.file = replacement;
        self.written = 0;
        self.rotate_at = MAX_BYTES;
        Ok(())
    }

    fn write_line(&mut self, line: &str) {
        if self.written >= self.rotate_at && self.rotate().is_err() {
            // Keep writing. §8.5's size cap guards disk usage; dropping the
            // line guards nothing and costs the record. An oversized log beats
            // no log, and the next attempt is a megabyte away rather than on
            // the very next line.
            self.rotate_at = self.written.saturating_add(ROTATE_RETRY_BYTES);
            // Said in the file itself: this cannot go through `log`, which
            // would re-enter the mutex this runs under, and a reader looking
            // at a 40 MB log deserves to know why it is 40 MB.
            let note = format!(
                "{{\"ts\":\"{}\",\"level\":\"error\",\"process\":\"log\",\
                 \"component\":\"yspot_log\",\"message\":\"could not rotate {}; \
                 still writing to it, so it will exceed the {} MB cap\"}}\n",
                timestamp(),
                self.path.display().to_string().replace('\\', "\\\\"),
                MAX_BYTES / (1024 * 1024)
            );
            if self.file.write_all(note.as_bytes()).is_ok() {
                self.written += note.len() as u64;
            }
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
    // Before any file is touched. A second call cannot install a logger, and
    // opening the path anyway would create an empty log file and immediately
    // abandon it — plus a directory tree to hold it.
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }

    let requested = std::env::var("RUST_LOG").ok();
    let level = requested
        .as_deref()
        .and_then(|v| v.parse::<LevelFilter>().ok())
        .unwrap_or(LevelFilter::Info);
    // A value that did not parse is remembered, not shrugged off. This
    // logger takes a bare level; `env_logger`, which the service used to
    // use, also took per-module filters like `yspot_indexd=debug`. Anyone
    // reaching for that spelling out of habit would otherwise turn debug
    // logging ON, see none of it, and conclude the problem is not being
    // logged — the exact wrong conclusion, arrived at silently.
    let unparsed = requested.filter(|v| v.parse::<LevelFilter>().is_err());
    let mut where_to = None;
    let file = path.and_then(|p| match Rotating::open(p.clone()) {
        Ok(r) => {
            where_to = Some(p);
            Some(r)
        }
        Err(e) => {
            // Not `eprintln!`, which panics on a write error. Stderr
            // redirected to a pipe whose reader has exited is a broken pipe,
            // and dying here would defeat the entire point of this arm:
            // losing logs is bad, refusing to start is worse.
            let _ = writeln!(
                std::io::stderr(),
                "log: cannot open {} ({e}); stderr only",
                p.display()
            );
            None
        }
    });
    let logger = Box::new(FileLogger {
        process,
        file: Mutex::new(file),
        stderr: true,
    });
    // Only announce a run we actually took charge of. A second call cannot
    // install anything — `log` allows one logger per process — so its banner
    // would go to the FIRST logger and describe a file and a level that call
    // never put in force. A line saying a run started, in the log of a run
    // that did not, is precisely the kind of confident wrong statement the
    // rest of this crate exists to avoid.
    if log::set_boxed_logger(logger).is_err() {
        return;
    }
    log::set_max_level(level);

    // Not `log::info!`. The macros consult `max_level`, so `RUST_LOG=warn` —
    // an ordinary way to quieten a noisy tool — would drop the banner and take
    // every run boundary in the file with it, along with §8.5's crash-free
    // metric, which is counted per run. The docstring above promises no
    // process can forget this line; going through the logger directly is what
    // makes that true rather than true-at-the-default-level.
    //
    // `Off` is still honoured. That one is not a side effect of turning the
    // volume down, it is someone asking for silence.
    if level != LevelFilter::Off {
        emit(
            Level::Info,
            &format!(
                "{RUN_START}: {process} {version}, pid {}, {}",
                std::process::id(),
                match &where_to {
                    Some(p) => format!("log {}", p.display()),
                    None => "no log file; stderr only".to_string(),
                }
            ),
        );
        if let Some(v) = unparsed {
            emit(
                Level::Warn,
                &format!(
                    "RUST_LOG={v:?} is not a level this logger understands, so it \
                     was ignored and the level is {level}. Use one of: error, \
                     warn, info, debug, trace."
                ),
            );
        }
    }
}

/// Write one record straight to the installed logger, past `max_level`.
///
/// For the handful of lines whose absence would make the file harder to read
/// than a missing level would make it noisier: the run boundary, and the
/// warning that the level itself was misconfigured.
fn emit(level: Level, message: &str) {
    log::logger().log(
        &Record::builder()
            .args(format_args!("{message}"))
            .level(level)
            .target(module_path!())
            .build(),
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
