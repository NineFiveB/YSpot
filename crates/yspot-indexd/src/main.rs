//! yspot-indexd — M0 console-mode index service (SPEC §3, §4).
//!
//! M0 runs as a console process (the real Windows service wrapper is M1):
//!   yspot-indexd --walk <path>   unelevated dev mode: filesystem walk
//!   yspot-indexd --mft  <C:>     MFT enumeration + USN tailing (elevated)
//!
//! Common behavior: §8.5's structured log (RUST_LOG, default info) to
//! `%ProgramData%\YSpot\logs\indexd.log` AND the console; enumeration
//! timing, entry count, and ram_bytes are logged at completion — those are
//! the M0 exit-criteria numbers (§10).
//!
//! The file half is what makes the console mode survivable for a fortnight.
//! A console is a buffer: it scrolls, it dies with its window, and a service
//! that failed at 03:00 has left nothing to read by the time anyone looks.
//! §8.5 puts the service's log under `%ProgramData%` rather than the shell's
//! `%LOCALAPPDATA%` because the directory follows the writer's privilege, and
//! it is world-READABLE so that exporting it needs no elevation (§5.9).

mod idx_api;
mod pipe;
mod session;
mod state;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use idx_api::UsnCursor;
use state::{Mode, ServiceState, VS_TAILING};

const USAGE: &str = "usage: yspot-indexd --walk <path> | --mft <C:>";

/// §8.5's log location for the service: `%ProgramData%\YSpot\logs`.
///
/// `None` if the variable is missing, which leaves the console as the only
/// output rather than inventing a directory. Nothing here creates the ACL
/// §8.5 specifies — that is the service MSI's job (§9.1), and until it
/// exists the directory inherits `%ProgramData%`'s own, which is already
/// world-readable and admin-writable. Close enough to run on, not close
/// enough to ship: recorded in docs/M1.md.
fn log_path() -> Option<std::path::PathBuf> {
    std::env::var_os("ProgramData").map(|b| {
        std::path::PathBuf::from(b)
            .join("YSpot")
            .join("logs")
            .join("indexd.log")
    })
}

fn main() {
    yspot_log::init("indexd", env!("CARGO_PKG_VERSION"), log_path());
    // After the logger, so the hook has somewhere to write.
    yspot_log::install_panic_hook();

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
            log_ram_breakdown(&idx);

            let state = Arc::new(ServiceState::new(idx, root, Mode::Walk));
            // Dev mode has no USN tailing; the index is as live as it gets.
            state.set_vol_state(VS_TAILING);
            spawn_housekeeping(state.clone(), HOUSEKEEPING_LOST_WALK);
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
            log_ram_breakdown(&idx);

            let state = Arc::new(ServiceState::new(idx, root, Mode::Mft));
            state.set_vol_state(VS_TAILING);
            spawn_usn_tail(state.clone(), drive, cursor);
            spawn_housekeeping(state.clone(), HOUSEKEEPING_LOST_MFT);
            pipe::serve(state)
        }
    }
}

/// One line of per-structure memory attribution after enumeration, in
/// B/entry, with the mean name length every arena-sized row scales through
/// (issue #10: the §3.4 accounting is a function of that length, and the
/// reference-machine runs need the real figure next to the total).
fn log_ram_breakdown(idx: &idx_api::VolumeIndex) {
    let n = idx_api::entry_count(idx).max(1) as f64;
    let b = idx_api::ram_breakdown(idx);
    let per = |v: u64| v as f64 / n;
    log::info!(
        "ram_bytes by structure (B/entry): entries={:.2} name_arena={:.2} folded_arena={:.2} \
         frn_map={:.2} rank_key={:.2} arena_recs={:.2} owner={:.2} initials={:.2} head={:.2} \
         charclass_bsi={:.2} presence={:.2} free_slots={:.2} depth_repair={:.2} total={:.2}; \
         mean name length {:.1} B",
        per(b.entries),
        per(b.name_arena),
        per(b.folded_arena),
        per(b.frn_map),
        per(b.rank_key),
        per(b.arena_recs),
        per(b.owner),
        per(b.initials),
        per(b.head),
        per(b.charclass_bsi),
        per(b.presence),
        per(b.free_slots),
        per(b.depth_repair_state),
        per(b.total()),
        idx_api::name_arena_len(idx) as f64 / n,
    );
}

/// How often the housekeeping thread looks for deferred writer-side work.
///
/// Sized under the index's own ~500 ms depth-repair debounce, so the debounce
/// — not the poll — is what decides when a repair runs. A tick that finds
/// nothing to do is one atomic-free read-lock acquisition and a compare.
const HOUSEKEEPING_TICK: Duration = Duration::from_millis(250);

/// Compact the index when it has accumulated enough dead weight (§3.7).
///
/// Gated on machine idle (§3.6) because compaction takes the write lock for
/// its whole run — every structure is renumbered at once, so unlike the depth
/// repair it cannot be sliced. `must_compact` is the escape hatch: past 40%
/// dead bytes the index is wasting more than idleness is worth waiting for, so
/// it runs regardless.
fn maybe_compact(state: &ServiceState) {
    let (should, must) = {
        let idx = state.index_read();
        (idx.should_compact(), idx.must_compact())
    };
    if !should && !must {
        return;
    }
    if !must && !state.machine_idle() {
        return; // a session is interactive; wait for a quiet window
    }

    // Not a save/restore of `vol_state`: a re-enumeration on the USN thread can
    // start and finish inside this window, and writing back the value read on
    // the way in would bury the `Tailing` it published — leaving the volume
    // reporting "Rebuilding" with nothing left to clear it.
    let busy = state.rebuilding();
    let t0 = Instant::now();
    let (reclaimed, entries) = {
        let mut idx = state.index_write();
        (idx.compact(), idx.len())
    };
    drop(busy);
    log::info!(
        "compacted: reclaimed {reclaimed} bytes over {entries} entries in {} ms{}",
        t0.elapsed().as_millis(),
        if must {
            " (forced: past the hard threshold)"
        } else {
            ""
        }
    );
}

/// What a dead housekeeping thread costs, which is not the same in both modes.
///
/// Compaction stops either way: this thread is its only caller. What differs
/// is whether that matters. In MFT mode the index is live, so unreclaimed
/// space accumulates and depth repair falls back to the tail thread's cadence
/// — which is event-driven, so a quiet volume can hold stale ranking depth
/// indefinitely. In walk mode the index is built once by `walk_into` and never
/// mutated again: nothing reparents, so depth cannot drift, and nothing is
/// deleted, so there is no space to reclaim.
///
/// Saying "depth repair has stopped, ranking will drift" in both was false in
/// both directions — it understated the MFT case and invented the walk one,
/// sending a reader hunting for drift that cannot occur.
const HOUSEKEEPING_LOST_MFT: &str = concat!(
    "compaction has stopped, so the index will never reclaim the space ",
    "deleted files left. Depth repair still runs on the tail thread, but only ",
    "when the journal produces an event, so ranking depth on a quiet volume ",
    "can now stay stale indefinitely"
);
const HOUSEKEEPING_LOST_WALK: &str = concat!(
    "in walk mode this costs nothing measurable: the index is built once and ",
    "never mutated again, so there is no reparenting for depth repair to ",
    "correct and no deleted entry for compaction to reclaim"
);

/// Deferred writer-side maintenance, on a clock of its own.
///
/// This exists because the USN tail thread is NOT a clock. `read_batch` blocks
/// until the journal produces data, so on a quiescent volume — precisely the
/// case where a `move` is the last thing that happened — a repair armed by that
/// move would wait for the next unrelated filesystem event before anything
/// looked at it. Not "a few hundred milliseconds of stale ranking depth"
/// (behavior change 4), but unbounded. The tail loop keeps polling too, since
/// it is already holding the write lock for its batch, but nothing depends on
/// it any more.
///
/// Step 9's in-place compaction wants the same cadence (its triggers are
/// fractions of garbage that only a periodic look can notice), so it lands
/// here rather than on the tail thread.
fn spawn_housekeeping(state: Arc<ServiceState>, lost: &'static str) {
    std::thread::Builder::new()
        .name("housekeeping".into())
        .spawn(move || {
            supervise("housekeeping", lost, || loop {
                std::thread::sleep(HOUSEKEEPING_TICK);
                // §3.6 pause suspends index maintenance, not just tailing.
                if !state.is_paused() {
                    repair_depths(&state);
                    compact_or_die(&state);
                }
            })
        })
        .expect("spawning housekeeping thread");
}

/// Run a thread body that is supposed to run forever, and say so if it stops.
///
/// The two maintenance threads are spawned and never joined, which is right —
/// nothing has anything to wait for. It also means that if one of them dies,
/// NOTHING notices. The service keeps accepting connections and keeps
/// answering searches, from an index that has quietly stopped being
/// maintained, and the only symptom is results that are subtly wrong in a way
/// no one can date. That is the worst failure a two-week dogfood can suffer:
/// it does not look like a failure, so it does not get reported, and the log
/// is what has to say otherwise.
///
/// Deliberately no restart. A panic here means a bug, and a bug that recurs
/// every tick produces a log full of noise and a service that looks healthy
/// while spinning. Recording it once, loudly, is the honest answer; deciding
/// what to do about it needs a person who has read the line.
///
/// Surviving is only the right answer because the two bodies leave the index
/// INTACT when they stop — frozen or unmaintained, but internally consistent
/// and safe to read. The one operation for which that is false is compaction,
/// which is why it does not come through here: see [`compact_or_die`].
fn supervise(name: &'static str, consequence: &'static str, body: impl FnOnce()) {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
    // Both arms are failures: these bodies are infinite loops by construction,
    // so a clean return is as wrong as an unwind, just quieter.
    log::error!("{}", epitaph(name, outcome.is_err(), consequence));
    log::logger().flush();
}

/// The line [`supervise`] writes. Separate so it can be tested without
/// installing a global logger.
fn epitaph(name: &str, panicked: bool, consequence: &str) -> String {
    let how = if panicked { "panicked" } else { "returned" };
    format!(
        "the {name} thread {how} and will not run again. {consequence}. The \
         service is otherwise still up, so this will not look broken: restart it."
    )
}

/// Compact, and take the process down if compaction unwinds.
///
/// Everywhere else in this service, surviving a panic is the better answer.
/// Here it is the worse one, and the difference is that `compact()` is
/// **in place**. It renumbers roughly ten parallel structures in sequence to
/// avoid a ~42 MB transient copy, and the `frn_map` remap is the last step,
/// some hundred and fifty lines after the entry table is truncated. A panic
/// anywhere in that window leaves slot ids from before the compaction pointing
/// into a table from after it — and, because two of the arenas are rewritten
/// through `as_mut_vec`, possibly bytes that are not valid UTF-8.
///
/// Nothing downstream would notice. The write lock's poisoning is discarded on
/// purpose (`state.rs`), on the stated grounds that "the index is never left
/// half-written by our writers" — true of every writer except this one, which
/// arrived after that comment. So the torn index stays published, the pipe
/// keeps accepting, and every subsequent search either panics its own session
/// thread or, worse, quietly returns nothing.
///
/// A service answering searches from an index it cannot vouch for is the one
/// failure this whole diagnostic effort exists to prevent, and it is not
/// something a log line can mitigate: by the time anyone reads it, the wrong
/// answers have already been believed. So this one dies, and says why. The
/// index is rebuilt from the MFT on the next start, which costs seconds.
fn compact_or_die(state: &Arc<ServiceState>) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| maybe_compact(state))).is_ok() {
        return;
    }
    log::error!(
        "compaction panicked, which leaves the index renumbered in part and \
         internally inconsistent. Continuing would serve wrong answers from it \
         and never say so, which is worse than stopping, so the service is \
         exiting. It rebuilds from the MFT on the next start. The panic itself \
         is logged above."
    );
    log::logger().flush();
    std::process::exit(1);
}

/// Every consequence string the service can emit, so the tripwire below
/// cannot silently stop covering one. A new epitaph that is not added here is
/// a new epitaph nothing checks.
#[cfg(test)]
const ALL_CONSEQUENCES: [(&str, &str); 3] = [
    ("housekeeping/mft", HOUSEKEEPING_LOST_MFT),
    ("housekeeping/walk", HOUSEKEEPING_LOST_WALK),
    ("usn-tail", USN_TAIL_LOST),
];

/// What a dead USN tail thread costs.
///
/// A named constant so the run-of-spaces tripwire can reach it. It was an
/// inline argument, which is exactly why the first version of that test missed
/// the one string that had ever carried the defect.
const USN_TAIL_LOST: &str = concat!(
    "the index is frozen at this moment: searches keep answering, from data ",
    "that stops here and silently ages"
);

fn spawn_usn_tail(state: Arc<ServiceState>, drive: String, cursor: UsnCursor) {
    std::thread::Builder::new()
        .name("usn-tail".into())
        .spawn(move || {
            supervise("usn-tail", USN_TAIL_LOST, || {
                usn_tail_loop(state, drive, cursor)
            })
        })
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
                    // Opportunistic only. This thread is NOT the depth repair's
                    // clock — `read_batch` blocks until the journal produces
                    // data, so on a quiescent volume nothing would come back
                    // here at all. `spawn_housekeeping` owns the cadence; this
                    // call just takes the chance to drain a repair while the
                    // batch has already warmed the index.
                    let n = events.len();
                    if n > 0 {
                        let mut idx = state.index_write();
                        for ev in events {
                            idx_api::apply_usn(&mut idx, ev);
                        }
                        drop(idx);
                        log::debug!("applied {n} USN events (next usn {})", cursor.next_usn);
                    }
                    repair_depths(&state);
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

/// Drain an outstanding ranking-depth repair off the query path, one slice per
/// write-lock acquisition (§3.4 `depth_penalty`, design §5).
///
/// A directory that changes parent shifts every descendant's cached depth by a
/// constant. The alternative — refreshing depth from the query path — would put
/// an O(n) sweep inside the §2.5 10 ms budget on the first keystroke after any
/// `move`, which is exactly the rebuild-on-mutation cliff the cached column
/// exists to remove. Here it costs readers nothing: the lock is released between
/// slices, so a search never waits for the whole sweep, and until it lands the
/// only effect is a few percent of score on a moved subtree. No result appears
/// or disappears.
///
/// Called from the housekeeping thread on a fixed tick, and opportunistically
/// from the USN tail loop. It must stay safe to call from either — the guard
/// below and `repair_depths_slice` are both no-ops when nothing is armed.
fn repair_depths(state: &ServiceState) {
    // Read lock for the poll: it runs on every tick and after every USN batch
    // and almost always says no, so it must not contend with searches.
    if !idx_api::depth_repair_due(&state.index_read()) {
        return;
    }
    let t0 = Instant::now();
    let mut slices = 0usize;
    while idx_api::repair_depths_slice(&mut state.index_write()) {
        slices += 1;
    }
    log::debug!(
        "ranking-depth repair done in {} slice(s), {} ms",
        slices + 1,
        t0.elapsed().as_millis()
    );
}

/// Full re-enumeration behind the serving index: build a fresh index without
/// holding the lock, then swap it in and bump the epoch (§3.3 wrap recovery,
/// §4.3 index_epoch). Retries forever — without a rebuilt index the volume
/// would serve stale data.
fn rebuild(state: &ServiceState, drive: &str) -> UsnCursor {
    // Held across every retry: the volume is being rebuilt for as long as this
    // loop runs, and a failed attempt that slept for five seconds is still not
    // a volume anyone should be told is tailing.
    let _busy = state.rebuilding();
    loop {
        let t0 = Instant::now();
        let mut fresh = idx_api::new_index(0, &state.root_path);
        match idx_api::mft_enumerate(drive, &mut fresh) {
            Ok(cur) => {
                let (n, ram) = (idx_api::entry_count(&fresh), idx_api::ram_bytes(&fresh));
                *state.index_write() = fresh; // short write lock: swap only
                let epoch = state.index_epoch.fetch_add(1, Ordering::SeqCst) + 1;
                // The lifecycle underneath the overlay: from here on the
                // volume is tailing, which is what shows once `_busy` drops.
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
    let t = s.trim().trim_end_matches(['\\', '/']);
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
    /// A maintenance thread that dies must not take the service with it, and
    /// must not die quietly either.
    #[test]
    fn supervise_catches_a_panic_and_keeps_going() {
        let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = ran.clone();
        // Spawned, because that is how it is used: a panic escaping a spawned
        // closure aborts nothing but its own thread, so the assertion that
        // matters is that the JOIN succeeds rather than reporting an unwind.
        let h = std::thread::spawn(move || {
            super::supervise("test-thread", "nothing, this is a test", move || {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                panic!("the body blew up");
            })
        });
        assert!(h.join().is_ok(), "the panic escaped supervise");
        assert!(
            ran.load(std::sync::atomic::Ordering::SeqCst),
            "the body never ran, so the test proved nothing"
        );
    }

    /// No epitaph may contain a run of spaces.
    ///
    /// A lost `\` line-continuation puts the next line's indentation INSIDE
    /// the string, and the result reads like corruption in the one log line
    /// the runbook tells a reader to go and find. It has happened twice: once
    /// when these strings were written and once when the fix for that was
    /// applied and silently did not take. Neither rustfmt nor clippy has a
    /// lint for it, so this is the tripwire. `concat!` is the shape that
    /// cannot regress.
    #[test]
    fn no_epitaph_contains_a_run_of_spaces() {
        for (name, text) in super::ALL_CONSEQUENCES {
            assert!(
                !text.contains("  "),
                "{name} consequence has a run of spaces: {text:?}"
            );
        }
        for panicked in [true, false] {
            for (name, text) in super::ALL_CONSEQUENCES {
                let line = super::epitaph(name, panicked, text);
                assert!(
                    !line.contains("  "),
                    "composed epitaph has a run of spaces: {line:?}"
                );
            }
        }
    }

    /// A body that RETURNS is as much a failure as one that panics: both of
    /// these threads are infinite loops, so either way the work has stopped.
    #[test]
    fn a_clean_return_is_reported_too() {
        let quiet = super::epitaph("housekeeping", false, "compaction has stopped");
        assert!(quiet.contains("returned"), "{quiet}");
        let loud = super::epitaph("usn-tail", true, "the index is frozen");
        assert!(loud.contains("panicked"), "{loud}");

        // The consequence is the half a reader acts on, and the line has to
        // say the service is still up — otherwise "it seems fine" reads as
        // evidence against the log rather than as the symptom it is.
        assert!(loud.contains("the index is frozen"), "{loud}");
        assert!(loud.contains("restart"), "{loud}");
        assert!(loud.contains("usn-tail"), "{loud}");
    }

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
