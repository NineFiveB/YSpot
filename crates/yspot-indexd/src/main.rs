//! yspot-indexd — M0 console-mode index service (SPEC §3, §4).
//!
//! M0 runs as a console process (the real Windows service wrapper is M1):
//!   yspot-indexd --walk <path>   unelevated dev mode: filesystem walk
//!   yspot-indexd --mft  <C:>     MFT enumeration + USN tailing (elevated)
//!
//! Common behavior: env_logger (RUST_LOG, default info); enumeration timing,
//! entry count, and ram_bytes are logged at completion — those are the M0
//! exit-criteria numbers (§10).

mod idx_api;
mod pipe;
mod session;
mod state;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use idx_api::UsnCursor;
use state::{Mode, ServiceState, VS_REBUILDING, VS_TAILING};

const USAGE: &str = "usage: yspot-indexd --walk <path> | --mft <C:>";

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match parse_args(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };

    if let Err(e) = run(cli) {
        log::error!("fatal: {e:#}");
        std::process::exit(1);
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Cli {
    /// Normalized root with trailing backslash.
    Walk(String),
    /// Normalized drive designator, e.g. `C:`.
    Mft(String),
}

fn run(cli: Cli) -> anyhow::Result<()> {
    match cli {
        Cli::Walk(root) => {
            log::info!("M0 console mode: filesystem walk of {root} (unelevated dev mode)");
            let mut idx = idx_api::new_index(0, &root);
            let t0 = Instant::now();
            idx_api::walk_into(&root, &mut idx)?;
            let ms = t0.elapsed().as_millis();
            let (n, ram) = (idx_api::entry_count(&idx), idx_api::ram_bytes(&idx));
            // M0 exit-criteria numbers (§10): duration, entries, resident bytes.
            log::info!("walk complete: {n} entries in {ms} ms, ram_bytes={ram}");

            let state = Arc::new(ServiceState::new(idx, root, Mode::Walk));
            // Dev mode has no USN tailing; the index is as live as it gets.
            state.set_vol_state(VS_TAILING);
            pipe::serve(state)
        }
        Cli::Mft(drive) => {
            let root = format!("{drive}\\");
            log::info!("M0 console mode: MFT enumeration of {drive} (needs elevation)");
            let mut idx = idx_api::new_index(0, &root);
            let t0 = Instant::now();
            let cursor = match idx_api::mft_enumerate(&drive, &mut idx) {
                Ok(c) => c,
                Err(e) if idx_api::is_access_denied(&e) => {
                    log::error!(
                        "MFT enumeration of {drive} was denied (os error 5). Opening a volume \
                         handle needs Administrators/Backup Operators rights (§3.2) — start \
                         yspot-indexd from an ELEVATED prompt, or use `--walk <path>` for \
                         unelevated dev mode."
                    );
                    std::process::exit(3);
                }
                Err(e) => return Err(e),
            };
            let ms = t0.elapsed().as_millis();
            let (n, ram) = (idx_api::entry_count(&idx), idx_api::ram_bytes(&idx));
            // M0 exit-criteria numbers (§10): duration, entries, resident bytes.
            log::info!("MFT enumeration complete: {n} entries in {ms} ms, ram_bytes={ram}");

            let state = Arc::new(ServiceState::new(idx, root, Mode::Mft));
            state.set_vol_state(VS_TAILING);
            spawn_usn_tail(state.clone(), drive, cursor);
            pipe::serve(state)
        }
    }
}

fn spawn_usn_tail(state: Arc<ServiceState>, drive: String, cursor: UsnCursor) {
    std::thread::Builder::new()
        .name("usn-tail".into())
        .spawn(move || usn_tail_loop(state, drive, cursor))
        .expect("spawning usn-tail thread");
}

/// USN journal tailing (§3.3): apply batches under the write lock; on journal
/// wrap/truncation, rebuild by full re-enumeration. Pause (§3.6) is honored
/// between batches.
fn usn_tail_loop(state: Arc<ServiceState>, drive: String, mut cursor: UsnCursor) {
    log::info!(
        "USN tailing {drive} from usn {} (journal {:#x})",
        cursor.next_usn,
        cursor.journal_id
    );
    'reopen: loop {
        let mut tailer = match idx_api::usn_open(&drive) {
            Ok(t) => t,
            Err(e) => {
                log::error!("USN open failed ({e:#}); retrying in 2 s");
                std::thread::sleep(Duration::from_secs(2));
                continue 'reopen;
            }
        };
        loop {
            if state.is_paused() {
                // PauseIndexing (§3.6): the tail thread idles between batches.
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
            // read_batch advances `cursor` on success; wrap/truncation comes
            // back as NeedsRebuild, never as Err (§3.3 recovery contract).
            match idx_api::usn_read_batch(&mut tailer, &mut cursor) {
                Ok(idx_api::TailOutcome::Events(events)) => {
                    if events.is_empty() {
                        continue;
                    }
                    let n = events.len();
                    {
                        let mut idx = state.index_write();
                        for ev in events {
                            idx_api::apply_usn(&mut idx, ev);
                        }
                    }
                    log::debug!("applied {n} USN events (next usn {})", cursor.next_usn);
                }
                Ok(idx_api::TailOutcome::NeedsRebuild(reason)) => {
                    log::warn!("USN journal wrap ({reason}); full re-enumeration (§3.3)");
                    cursor = rebuild(&state, &drive);
                    continue 'reopen;
                }
                Err(e) => {
                    log::error!("USN read failed ({e:#}); retrying in 1 s");
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        }
    }
}

/// Full re-enumeration behind the serving index: build a fresh index without
/// holding the lock, then swap it in and bump the epoch (§3.3 wrap recovery,
/// §4.3 index_epoch). Retries forever — without a rebuilt index the volume
/// would serve stale data.
fn rebuild(state: &ServiceState, drive: &str) -> UsnCursor {
    loop {
        state.set_vol_state(VS_REBUILDING);
        let t0 = Instant::now();
        let mut fresh = idx_api::new_index(0, &state.root_path);
        match idx_api::mft_enumerate(drive, &mut fresh) {
            Ok(cur) => {
                let (n, ram) = (idx_api::entry_count(&fresh), idx_api::ram_bytes(&fresh));
                *state.index_write() = fresh; // short write lock: swap only
                let epoch = state.index_epoch.fetch_add(1, Ordering::SeqCst) + 1;
                state.set_vol_state(VS_TAILING);
                log::info!(
                    "re-enumeration complete: {n} entries in {} ms, ram_bytes={ram}, epoch={epoch}",
                    t0.elapsed().as_millis()
                );
                return cur;
            }
            Err(e) => {
                log::error!("re-enumeration failed ({e:#}); retrying in 5 s");
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    }
}

fn parse_args(args: &[String]) -> Result<Cli, String> {
    match args {
        [flag, value] if flag.as_str() == "--walk" => {
            if value.trim().is_empty() {
                return Err("--walk expects a directory path".into());
            }
            Ok(Cli::Walk(normalize_root(value)))
        }
        [flag, value] if flag.as_str() == "--mft" => parse_drive(value)
            .map(Cli::Mft)
            .ok_or_else(|| format!("--mft expects a drive like C:, got {value:?}")),
        _ => Err("expected exactly one mode flag".into()),
    }
}

/// Walk root: forward slashes normalized, trailing backslash guaranteed
/// (`root_path` contract of the index).
fn normalize_root(p: &str) -> String {
    let mut s = p.trim().replace('/', "\\");
    if !s.ends_with('\\') {
        s.push('\\');
    }
    s
}

/// Accepts `C`, `c:`, `C:\` … and yields the canonical `C:`.
fn parse_drive(s: &str) -> Option<String> {
    let t = s.trim().trim_end_matches(|c| c == '\\' || c == '/');
    let mut chars = t.chars();
    let letter = chars.next()?;
    if !letter.is_ascii_alphabetic() {
        return None;
    }
    match chars.next() {
        None => {}
        Some(':') if chars.next().is_none() => {}
        _ => return None,
    }
    Some(format!("{}:", letter.to_ascii_uppercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_walk() {
        assert_eq!(
            parse_args(&v(&["--walk", "C:/Users/x"])),
            Ok(Cli::Walk("C:\\Users\\x\\".into()))
        );
        assert_eq!(
            parse_args(&v(&["--walk", "C:\\data\\"])),
            Ok(Cli::Walk("C:\\data\\".into()))
        );
    }

    #[test]
    fn parse_mft() {
        assert_eq!(parse_args(&v(&["--mft", "C:"])), Ok(Cli::Mft("C:".into())));
        assert_eq!(parse_args(&v(&["--mft", "c"])), Ok(Cli::Mft("C:".into())));
        assert_eq!(
            parse_args(&v(&["--mft", "d:\\"])),
            Ok(Cli::Mft("D:".into()))
        );
        assert!(parse_args(&v(&["--mft", "CD:"])).is_err());
        assert!(parse_args(&v(&["--mft", "1:"])).is_err());
    }

    #[test]
    fn parse_rejects_bad_shapes() {
        assert!(parse_args(&v(&[])).is_err());
        assert!(parse_args(&v(&["--walk"])).is_err());
        assert!(parse_args(&v(&["--mft"])).is_err());
        assert!(parse_args(&v(&["--walk", "a", "b"])).is_err());
        assert!(parse_args(&v(&["--unknown", "x"])).is_err());
    }

    #[test]
    fn root_normalization() {
        assert_eq!(normalize_root("C:/a/b"), "C:\\a\\b\\");
        assert_eq!(normalize_root("C:\\a\\b\\"), "C:\\a\\b\\");
        assert_eq!(normalize_root(" C:\\a "), "C:\\a\\");
    }

    #[test]
    fn drive_parsing() {
        assert_eq!(parse_drive("C:"), Some("C:".into()));
        assert_eq!(parse_drive("c"), Some("C:".into()));
        assert_eq!(parse_drive("C:\\"), Some("C:".into()));
        assert_eq!(parse_drive("C:/"), Some("C:".into()));
        assert_eq!(parse_drive(""), None);
        assert_eq!(parse_drive("C:x"), None);
        assert_eq!(parse_drive("::"), None);
    }
}
