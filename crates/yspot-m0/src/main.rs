//! yspot-m0 — the §10 M0 measurement harness.
//!
//! What the in-app HUD cannot prove, this measures from outside: input is
//! injected with `SendInput`, timestamped with QPC immediately before
//! injection, and correlated against the shell's ETW markers and
//! Microsoft-Windows-Dwm-Core composition events consumed in one QPC-clocked
//! session (`etw.rs`). §2.5's decomposition falls out per keystroke:
//!
//!   keydown ──(pipe, match, decode)──► `results` marker   ≤ 20 ms p95 gate
//!           ──(next rAF applies)─────► `applied` marker
//!           ──(compositor)───────────► DWM composition    "pixels"
//!
//! Where each endpoint sits, stated so the numbers are read honestly: the
//! `results` marker fires in the shell's pipe thread after the frame is
//! decoded — before the Tauri event relay and the WebView2 IPC hop — so the
//! gated column is "results decoded in the shell", a slight underestimate of
//! "available to the frontend". The `applied` marker crosses the
//! webview→shell invoke hop before it is stamped, so that column carries the
//! hop as overhead. The pixels endpoint is the first Dwm-Core composition
//! event after `applied`; on a build where `--dwm-ids` has not been pinned
//! from `etw-dump`, any composition-keyword event is accepted and every
//! DWM-gated verdict prints ADVISORY rather than PASS.
//!
//! Preconditions the numbers are only valid under: an otherwise-idle desktop
//! (composition events are attributed by time, and anything else animating
//! steals the attribution), hands off the keyboard (injected and physical
//! input interleave), and an elevated prompt (ETW session creation).
//!
//!   yspot-m0 toggle     [--cycles N] [--dwell-hidden-ms N] [--dwell-visible-ms N] [--dwm-ids a,b]
//!   yspot-m0 type       [--queries a,b,c] [--iterations N] [--dwm-ids a,b]
//!   yspot-m0 freshness  [--iterations N] [--dir PATH]
//!   yspot-m0 startup    [--exe PATH] [--drive C:] [--keep]
//!   yspot-m0 clipboard
//!   yspot-m0 etw-dump   [--seconds N]

mod clipboard;
mod etw;
mod input;
mod pipec;
mod stats;

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use etw::{Event, Session, DWM_KEYWORDS_DUMP, DWM_KEYWORDS_MEASURE};
use stats::{stats, Stats};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");
    let rest = &args[1.min(args.len())..];
    let r = match cmd {
        "toggle" => cmd_toggle(rest),
        "type" => cmd_type(rest),
        "freshness" => cmd_freshness(rest),
        "startup" => cmd_startup(rest),
        "clipboard" => clipboard::run(),
        "etw-dump" => cmd_etw_dump(rest),
        _ => {
            eprintln!(
                "usage: yspot-m0 <toggle|type|freshness|startup|clipboard|etw-dump> [options]\n\
                 see the module doc / docs/M0.md for options and preconditions"
            );
            std::process::exit(2);
        }
    };
    if let Err(e) = r {
        eprintln!("yspot-m0 {cmd}: {e:#}");
        std::process::exit(1);
    }
}

fn arg<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

/// `--dwm-ids 15,64` → the pinned composition-pass event IDs for this Windows
/// build, from a prior `etw-dump`. `None` = unpinned: any composition-keyword
/// event is accepted and DWM-gated verdicts are advisory.
fn dwm_ids(args: &[String]) -> Option<Vec<u16>> {
    arg(args, "--dwm-ids").map(|v| {
        v.split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect::<Vec<u16>>()
    })
}

fn dwm_allows(ids: &Option<Vec<u16>>, id: u16) -> bool {
    match ids {
        Some(ids) => ids.contains(&id),
        None => true,
    }
}

/// Parse `key=value` out of a marker string (`results gen=12 seq=0 final=1`).
fn marker_field(text: &str, key: &str) -> Option<u64> {
    let pat = format!("{key}=");
    text.split_whitespace()
        .find_map(|tok| tok.strip_prefix(&pat))
        .and_then(|v| v.parse().ok())
}

/// One printed row. `censored` counts steps that produced no sample for this
/// column (timeouts, missing endpoints): a gated column with censored steps
/// can never PASS — the tail a p95 gate exists to catch is exactly what a
/// timeout hides. `advisory` marks a verdict that cannot be trusted as a gate
/// yet (unpinned DWM IDs) and says why.
fn print_stats(
    label: &str,
    s: &Stats,
    budget_ms: Option<f64>,
    censored: usize,
    advisory: Option<&str>,
) {
    let verdict = match budget_ms {
        None => {
            if censored > 0 {
                format!("  ({censored} dropped)")
            } else {
                String::new()
            }
        }
        Some(b) if s.n == 0 => format!("  (budget p95 ≤ {b:.0} ms — NO SAMPLES)"),
        Some(b) => {
            let pf = if s.p95 <= b { "PASS" } else { "FAIL" };
            match (censored, advisory) {
                (0, None) => format!("  (budget p95 ≤ {b:.0} ms: {pf})"),
                (0, Some(why)) => format!("  (budget p95 ≤ {b:.0} ms: {pf} — ADVISORY, {why})"),
                (c, _) => format!(
                    "  (budget p95 ≤ {b:.0} ms: FAIL — {c} steps censored by timeout; \
                     a censored distribution cannot PASS"
                ),
            }
        }
    };
    println!(
        "{label:<28} n={:<4} min {:>7.2}  p50 {:>7.2}  p95 {:>7.2}  p99 {:>7.2}  max {:>7.2} ms{verdict}",
        s.n, s.min, s.p50, s.p95, s.p99, s.max
    );
}

/// Ensure the launcher process is running and in the given visibility state,
/// toggling it via the real Alt+Space path if needed.
///
/// The summon is RETRIED: injected Alt+Space is not perfectly reliable when
/// the harness has been hammering the input path (a whole toggle run before
/// a type run leaves it flaky), so up to six chords are sent, each confirmed
/// by the visibility poll rather than the marker — polling `IsWindowVisible`
/// is what the caller actually needs true, and it recovers even if the marker
/// was missed.
fn ensure_visible(session: &Session, want_visible: bool) -> Result<()> {
    if input::launcher_visible().is_none() {
        bail!(
            "no YSpot launcher window (title \"YSpot\", process yspot-shell) — start the shell \
             first (apps/shell: npm run build && cargo run -p yspot-shell --release)"
        );
    }
    for attempt in 0..6 {
        if input::launcher_visible() == Some(want_visible) {
            return Ok(());
        }
        session.drain();
        if !input::send_alt_space() {
            bail!("SendInput(Alt+Space) failed");
        }
        // Poll visibility for up to 1.5 s; the marker path is a fast exit but
        // the poll is the authority.
        let deadline = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < deadline {
            if input::launcher_visible() == Some(want_visible) {
                std::thread::sleep(Duration::from_millis(120));
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        log::warn!("ensure_visible({want_visible}): attempt {attempt} did not take");
    }
    bail!(
        "could not bring the launcher {} after six Alt+Space chords — injected-input \
         reliability limit (see docs/M0.md)",
        if want_visible { "up" } else { "down" }
    )
}

/// Endpoints of one measurement step, derived from a QPC-SORTED view of every
/// event the step collected. Real-time ETW merges per-CPU buffers and does not
/// promise cross-buffer timestamp order, so sequential consume-and-discard
/// waits can eat an out-of-order event a later wait needed; collecting first
/// and ordering by timestamp makes delivery order irrelevant.
///
/// Every field is a running MINIMUM qpc over the qualifying events, updated in
/// O(1) as each event streams in (see [`StepOut::feed`]). Taking the minimum
/// rather than the first-arrived is what makes it robust to real-time ETW's
/// bounded cross-buffer reordering — a qualifying event that arrives late but
/// timestamps earlier still lowers the endpoint — WITHOUT sorting a
/// tens-of-thousands-strong DWM firehose on every arrival.
#[derive(Default, Clone, Copy)]
struct StepOut {
    /// `results … final=1` marker after t0: (min qpc, its gen).
    results: Option<(i64, u64)>,
    /// `applied … final=1` marker whose gen matches `results` and qpc > it.
    applied: Option<i64>,
    /// First accepted Dwm-Core event after `applied`.
    pixels: Option<i64>,
    /// `shown` marker after t0 (toggle flow).
    shown: Option<i64>,
    /// First accepted Dwm-Core event after `shown` (toggle flow).
    shown_pixels: Option<i64>,
}

fn min_opt(slot: &mut Option<i64>, v: i64) {
    if slot.map(|cur| v < cur).unwrap_or(true) {
        *slot = Some(v);
    }
}

impl StepOut {
    /// Fold one event in. Causal order (marker written before the composition
    /// it triggers) means `results`/`applied`/`shown` are known before their
    /// dependent DWM events arrive, so the O(1) running-min updates below are
    /// exact in the normal case and off only by ETW's sub-millisecond
    /// reordering.
    fn feed(&mut self, ev: &Event, t0: i64, ids: &Option<Vec<u16>>) {
        match ev {
            Event::Marker { text, qpc } if *qpc > t0 => {
                if text.starts_with("results ") && marker_field(text, "final") == Some(1) {
                    if let Some(gen) = marker_field(text, "gen") {
                        if self.results.map(|(q, _)| *qpc < q).unwrap_or(true) {
                            self.results = Some((*qpc, gen));
                        }
                    }
                } else if text.starts_with("applied ") && marker_field(text, "final") == Some(1) {
                    if let Some((r_qpc, gen)) = self.results {
                        if *qpc > r_qpc && marker_field(text, "gen") == Some(gen) {
                            min_opt(&mut self.applied, *qpc);
                        }
                    }
                } else if text == "shown" {
                    min_opt(&mut self.shown, *qpc);
                }
            }
            Event::Dwm { qpc, id, .. } if dwm_allows(ids, *id) => {
                if let Some(a) = self.applied {
                    if *qpc > a {
                        min_opt(&mut self.pixels, *qpc);
                    }
                }
                if let Some(s) = self.shown {
                    if *qpc > s {
                        min_opt(&mut self.shown_pixels, *qpc);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Stream events until `done(out)` or timeout, folding each into `out` in O(1)
/// via [`StepOut::feed`] — no accumulation, no sorting. `on_dwm` observes every
/// Dwm-Core (id, keyword) for the id histogram the toggle flow prints.
///
/// This is what keeps up with the composition-keyword firehose: tens of
/// thousands of DWM events per cycle, each an O(1) fold, so the main thread
/// never falls behind and never misses the marker it is waiting for.
fn collect_step(
    session: &Session,
    t0: i64,
    timeout: Duration,
    ids: &Option<Vec<u16>>,
    done: impl Fn(&StepOut) -> bool,
    mut on_dwm: impl FnMut(u16, u64),
) -> StepOut {
    let deadline = Instant::now() + timeout;
    let mut out = StepOut::default();
    session.flush();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return out;
        }
        match session.rx.recv_timeout(left.min(Duration::from_millis(20))) {
            Ok(ev) => {
                if let Event::Dwm { id, keyword, .. } = &ev {
                    on_dwm(*id, *keyword);
                }
                out.feed(&ev, t0, ids);
                if done(&out) {
                    return out;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => session.flush(),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return out,
        }
    }
}

// ---------------------------------------------------------------------------
// toggle — hotkey→visible (< 50 ms p95 gate) and the hidden-rAF throttle report.

fn cmd_toggle(args: &[String]) -> Result<()> {
    let cycles: usize = arg(args, "--cycles")
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    let dwell_hidden = Duration::from_millis(
        arg(args, "--dwell-hidden-ms")
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000),
    );
    let dwell_visible = Duration::from_millis(
        arg(args, "--dwell-visible-ms")
            .and_then(|v| v.parse().ok())
            .unwrap_or(300),
    );
    // DWM is OFF unless asked for: its Composition firehose defers WM_HOTKEY
    // delivery on an elevated injector and censors the run. The gated endpoint
    // (`hotkey → shown`, the <50 ms proxy) is a marker and needs no DWM; only
    // the informational `hotkey → visible` pixels column does, so it is opt-in.
    let with_dwm = flag(args, "--with-dwm");
    let ids = dwm_ids(args);
    let advisory = if ids.is_none() {
        Some("DWM IDs unpinned — run etw-dump and pass --dwm-ids")
    } else {
        None
    };

    input::unlock_foreground();
    let session = Session::start(if with_dwm { DWM_KEYWORDS_MEASURE } else { 0 })?;
    let freq = input::qpf();
    ensure_visible(&session, false)?;
    std::thread::sleep(dwell_hidden);

    let mut to_shown = Vec::new();
    let mut to_present = Vec::new();
    let mut rafgaps: Vec<String> = Vec::new();
    let mut censored_shown = 0usize;
    let mut censored_present = 0usize;

    // DWM id histogram across the whole run: (count, keyword-mask union).
    // With shows as the only visible changes, ids whose count tracks the cycle
    // count are the composition-pass candidates — this is what `--dwm-ids`
    // gets pinned from, with no separate choreography.
    let dwm_hist: std::cell::RefCell<std::collections::BTreeMap<u16, (usize, u64)>> =
        Default::default();
    let note_dwm = |id: u16, keyword: u64| {
        let mut h = dwm_hist.borrow_mut();
        let e = h.entry(id).or_insert((0, 0));
        e.0 += 1;
        e.1 |= keyword;
    };

    for cycle in 0..cycles {
        session.drain();
        let t0 = input::qpc();
        if !input::send_alt_space() {
            bail!("SendInput(Alt+Space) failed");
        }
        // Done on the marker alone unless DWM is enabled; without it the pixels
        // endpoint would never arrive and every cycle would burn the full 2 s.
        let out = collect_step(
            &session,
            t0,
            Duration::from_secs(2),
            &ids,
            |o| o.shown.is_some() && (!with_dwm || o.shown_pixels.is_some()),
            &note_dwm,
        );
        match out.shown {
            Some(s) => to_shown.push(input::ticks_to_ms(s - t0, freq)),
            None => {
                censored_shown += 1;
                if with_dwm {
                    censored_present += 1;
                }
                log::warn!("cycle {cycle}: no `shown` marker");
                std::thread::sleep(Duration::from_millis(300));
                let _ = ensure_visible(&session, false);
                continue;
            }
        }
        if with_dwm {
            match out.shown_pixels {
                Some(p) => to_present.push(input::ticks_to_ms(p - t0, freq)),
                None => censored_present += 1,
            }
        }
        // The throttle probe reports on the first rAF after show; give it a
        // short tail window of its own.
        for ev in session.collect(Duration::from_millis(400)) {
            if let Event::Dwm { id, keyword, .. } = &ev {
                note_dwm(*id, *keyword);
            }
            if let Event::Marker { text, .. } = ev {
                if text.starts_with("rafgap ") {
                    rafgaps.push(text);
                }
            }
        }

        std::thread::sleep(dwell_visible);
        session.drain();
        let t0h = input::qpc();
        if !input::send_alt_space() {
            bail!("SendInput(Alt+Space) failed");
        }
        if session
            .wait_for(
                Duration::from_secs(2),
                |e| matches!(e, Event::Marker { text, qpc } if text == "hidden" && *qpc > t0h),
            )
            .is_none()
        {
            log::warn!("cycle {cycle}: no `hidden` marker");
            let _ = ensure_visible(&session, false);
        }
        std::thread::sleep(dwell_hidden);
    }

    println!(
        "toggle — {cycles} Alt+Space show cycles, dwell hidden {} ms / visible {} ms",
        dwell_hidden.as_millis(),
        dwell_visible.as_millis()
    );
    // `shown` is the gated proxy for hotkey→visible (they differ by one
    // composition, a few ms; the 50 ms budget has ample room). The true
    // pixels endpoint is printed only when --with-dwm supplied it.
    print_stats(
        "hotkey → shown (marker)",
        &stats(to_shown),
        Some(50.0),
        censored_shown,
        Some("shown-marker proxy for hotkey→visible; +1 composition for pixels, pass --with-dwm"),
    );
    if with_dwm {
        print_stats(
            "hotkey → visible (DWM)",
            &stats(to_present),
            Some(50.0),
            censored_present,
            advisory,
        );
    }
    println!();
    println!(
        "hidden-WebView2 rAF throttling (§10 M0 flagged item; reported by the frontend probe).\n\
         The first cycle's report covers the pre-run hidden interval — read it separately:"
    );
    if rafgaps.is_empty() {
        println!("  no rafgap markers — frontend probe not running (stale dist/?)");
    } else {
        for g in rafgaps.iter().take(12) {
            println!("  {g}");
        }
        if rafgaps.len() > 12 {
            println!("  … {} more", rafgaps.len() - 12);
        }
    }
    println!();
    if with_dwm {
        println!();
        println!(
            "DWM ids seen during measurement (keywords 0x{:X}); with {cycles} shows as the only \n\
             visible changes, ids counting ≈ the cycle count are the composition-pass candidates \n\
             to pin via --dwm-ids:",
            etw::DWM_KEYWORDS_MEASURE
        );
        for (id, (n, kw)) in dwm_hist.borrow().iter() {
            println!("  id {id:>4}: {n:>6}  keyword 0x{kw:X}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// type — per-keystroke §2.5 decomposition (results ≤ 20 ms p95 gate).

fn cmd_type(args: &[String]) -> Result<()> {
    // Default set covers the §10 M0 exit classes: substring and the worst-case
    // 2-char / 3-char queries (below the fuzzy floor / at it).
    let queries: Vec<String> = match arg(args, "--queries") {
        Some(v) => v.split(',').map(|s| s.trim().to_string()).collect(),
        None => ["re", "tt", "rep", "ttz", "report", "cargo"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
    };
    let iterations: usize = arg(args, "--iterations")
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    // DWM off by default — the gated endpoint here is `keydown → results`, a
    // marker; only the informational `pixels` column needs composition events,
    // and its firehose censors the run (see cmd_toggle). Opt in with --with-dwm.
    let with_dwm = flag(args, "--with-dwm");
    let ids = dwm_ids(args);
    let advisory = if ids.is_none() {
        Some("DWM IDs unpinned — run etw-dump and pass --dwm-ids")
    } else {
        None
    };

    input::unlock_foreground();
    let session = Session::start(if with_dwm { DWM_KEYWORDS_MEASURE } else { 0 })?;
    let freq = input::qpf();
    // A fresh hide→show cycle, not just "visible": the input keeps its text
    // across hide/show, and a re-show selects it all, so the first injected
    // character REPLACES any residual query instead of appending to it.
    ensure_visible(&session, false)?;
    ensure_visible(&session, true)?;

    /// Sample columns for one (query, prefix-length) bucket. Keystroke k of a
    /// query measures the k-char prefix — pooling them would gate "worst-case
    /// 2-char p95" on a mixture of 1..n-char queries, which is not that class.
    #[derive(Default)]
    struct Bucket {
        results: Vec<f64>,
        applied: Vec<f64>,
        raf_delta: Vec<f64>,
        pixels: Vec<f64>,
        censored: usize,
        pixels_censored: usize,
    }
    let mut buckets: std::collections::BTreeMap<(usize, usize), Bucket> = Default::default();

    for _ in 0..iterations {
        for (qi, q) in queries.iter().enumerate() {
            for (ci, ch) in q.chars().enumerate() {
                let b = buckets.entry((qi, ci + 1)).or_default();
                session.drain();
                let t0 = input::qpc();
                if !input::send_char(ch) {
                    bail!("SendInput({ch:?}) failed");
                }
                let out = collect_step(
                    &session,
                    t0,
                    Duration::from_secs(2),
                    &ids,
                    |o| {
                        o.results.is_some()
                            && o.applied.is_some()
                            && (!with_dwm || o.pixels.is_some())
                    },
                    |_, _| {},
                );
                let Some((r_qpc, _)) = out.results else {
                    b.censored += 1;
                    continue;
                };
                b.results.push(input::ticks_to_ms(r_qpc - t0, freq));
                let Some(a_qpc) = out.applied else {
                    b.censored += 1;
                    continue;
                };
                b.applied.push(input::ticks_to_ms(a_qpc - t0, freq));
                b.raf_delta.push(input::ticks_to_ms(a_qpc - r_qpc, freq));
                if with_dwm {
                    match out.pixels {
                        Some(p) => b.pixels.push(input::ticks_to_ms(p - t0, freq)),
                        None => b.pixels_censored += 1,
                    }
                }
            }
            // Escape clears a non-empty query (§5.7) without dispatching a
            // search, so there is no marker to await — just let the UI settle
            // and drop whatever events straggle in.
            input::send_escape();
            std::thread::sleep(Duration::from_millis(120));
            session.drain();
        }
    }

    println!(
        "type — {iterations} iterations/query, one sample per KEYSTROKE, bucketed by prefix length"
    );
    println!(
        "§2.5 gate is `results` (decoded in the shell; excludes the event relay to the webview).\n\
         `applied − results` is the §10 \"applied in the next rAF after arrival\" clause\n\
         (expect ≤ one frame period: ~16.7 ms at 60 Hz, ~8.3 ms at 120 Hz):"
    );
    for (qi, q) in queries.iter().enumerate() {
        println!("query {q:?}:");
        for ((bqi, plen), b) in buckets.range((qi, 0)..(qi + 1, 0)) {
            debug_assert_eq!(*bqi, qi);
            let prefix: String = q.chars().take(*plen).collect();
            println!("  prefix {prefix:?} ({plen} chars):");
            print_stats(
                "    keydown → results",
                &stats(b.results.clone()),
                Some(20.0),
                b.censored,
                None,
            );
            print_stats(
                "    keydown → applied (rAF)",
                &stats(b.applied.clone()),
                None,
                0,
                None,
            );
            print_stats(
                "    applied − results",
                &stats(b.raf_delta.clone()),
                None,
                0,
                None,
            );
            if with_dwm {
                print_stats(
                    "    keydown → pixels (DWM)",
                    &stats(b.pixels.clone()),
                    None,
                    b.pixels_censored,
                    advisory,
                );
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// freshness — USN event → searchable (< 1 s gate). Needs an --mft service.

fn cmd_freshness(args: &[String]) -> Result<()> {
    let iterations: usize = arg(args, "--iterations")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let dir = arg(args, "--dir")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);

    let mut pipe = pipec::Pipe::connect()?;
    let vols = pipe.status()?;
    if let Some(v) = vols.first() {
        println!(
            "service: volume {} state {:?}, {} files indexed",
            v.mounts.first().map(String::as_str).unwrap_or("?"),
            v.state,
            v.files_indexed
        );
    }

    let mut samples = Vec::new();
    for i in 0..iterations {
        // Lowercase, hyphen-separated: an exact-name query with no fold or
        // segmentation surprises.
        let name = format!("m0-fresh-{}-{i}.txt", std::process::id());
        let path = dir.join(&name);
        let t0 = Instant::now();
        std::fs::write(&path, b"m0").with_context(|| format!("create {}", path.display()))?;

        let deadline = t0 + Duration::from_secs(10);
        let mut found = false;
        while Instant::now() < deadline {
            let hits = pipe.search(&name, 8)?;
            if hits.iter().any(|h| h.name.eq_ignore_ascii_case(&name)) {
                samples.push(t0.elapsed().as_secs_f64() * 1000.0);
                found = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = std::fs::remove_file(&path);
        if !found {
            bail!(
                "created file never became searchable within 10 s. A --walk service cannot \
                 pass this (synthetic FRNs, no USN tailing) — run `yspot-indexd --mft C:` \
                 elevated, and create the file on that volume (--dir)."
            );
        }
        // Let the delete drain through the journal before the next round.
        std::thread::sleep(Duration::from_millis(100));
    }
    println!(
        "freshness — {iterations} create→searchable cycles in {} \
         (sample includes the create syscall, one search RTT, and ≤ 25 ms poll quantization \
         — all inside the budget, so the gate is conservative)",
        dir.display()
    );
    print_stats(
        "USN create → searchable",
        &stats(samples),
        Some(1000.0),
        0,
        None,
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// startup — initial index time (< 15 s at 1M gate), RSS (< 200 MB at 1M gate).

fn cmd_startup(args: &[String]) -> Result<()> {
    let exe = arg(args, "--exe").unwrap_or(r"target\release\yspot-indexd.exe");
    let drive = arg(args, "--drive").unwrap_or("C:");
    let keep = flag(args, "--keep");

    println!("spawning {exe} --mft {drive} (needs elevation)");
    let t0 = Instant::now();
    let mut child = std::process::Command::new(exe)
        .args(["--mft", drive])
        .stderr(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawn {exe}"))?;

    // Poll the pipe until the service answers and reports Tailing.
    let (elapsed, vols) = loop {
        if let Some(status) = child.try_wait()? {
            bail!("indexd exited during startup: {status} (denied? not elevated?)");
        }
        std::thread::sleep(Duration::from_millis(200));
        let Ok(mut pipe) = pipec::Pipe::connect() else {
            if t0.elapsed() > Duration::from_secs(300) {
                bail!("service never came up within 300 s");
            }
            continue;
        };
        let vols = pipe.status()?;
        if vols
            .iter()
            .any(|v| v.state == yspot_proto::VolumeState::Tailing)
        {
            break (t0.elapsed(), vols);
        }
    };

    // Warm the query path before sampling memory: RSS at the Tailing instant
    // has touched none of the accel columns, and §10's cap is about the
    // service as it runs, not as it boots.
    if let Ok(mut pipe) = pipec::Pipe::connect() {
        for q in ["re", "conf", "index", "zzqxjv"] {
            let _ = pipe.search(q, 32);
        }
    }
    let rss = process_rss(&child);
    println!();
    println!(
        "startup — spawn → volume Tailing: {:.2} s (§10 gate: < 15 s at 1M files on the SSD \
         reference machine)",
        elapsed.as_secs_f64()
    );
    for v in &vols {
        println!(
            "  volume {}: {} files indexed, ram_bytes.filename = {:.1} MB",
            v.mounts.first().map(String::as_str).unwrap_or("?"),
            v.files_indexed,
            v.ram_bytes.filename as f64 / (1024.0 * 1024.0)
        );
    }
    if let Some((ws, private)) = rss {
        println!(
            "  service RSS after warm queries: working set {:.1} MB, private {:.1} MB \
             (§10 gate: < 200 MB at 1M files)",
            ws as f64 / (1024.0 * 1024.0),
            private as f64 / (1024.0 * 1024.0)
        );
    }

    if keep {
        println!("--keep: leaving the service running for freshness/type runs");
        std::mem::forget(child);
    } else {
        let _ = child.kill();
        let _ = child.wait();
    }
    Ok(())
}

/// (working set, private/commit) bytes of the child.
fn process_rss(child: &std::process::Child) -> Option<(usize, usize)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    // SAFETY: zeroed EX counters struct is the documented input (cb set to its
    // size; the API fills the base or EX struct per cb); the handle is the
    // live child's.
    unsafe {
        let mut c: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        if K32GetProcessMemoryInfo(
            child.as_raw_handle(),
            &mut c as *mut PROCESS_MEMORY_COUNTERS_EX as *mut PROCESS_MEMORY_COUNTERS,
            c.cb,
        ) != 0
        {
            Some((c.WorkingSetSize, c.PrivateUsage))
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// etw-dump — characterize what this build emits, to pin the DWM present IDs.

fn cmd_etw_dump(args: &[String]) -> Result<()> {
    let seconds: u64 = arg(args, "--seconds")
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    let session = Session::start(DWM_KEYWORDS_DUMP)?;
    let freq = input::qpf();
    println!("dumping {seconds}s of marker + Dwm-Core events (wiggle the launcher meanwhile)…");
    let t_end = Instant::now() + Duration::from_secs(seconds);
    let mut first_qpc: Option<i64> = None;
    let mut dwm_counts: std::collections::BTreeMap<u16, (usize, u64)> = Default::default();
    while Instant::now() < t_end {
        session.flush();
        match session.rx.recv_timeout(Duration::from_millis(100)) {
            Ok(ev) => {
                let base = *first_qpc.get_or_insert(ev.qpc());
                let ms = input::ticks_to_ms(ev.qpc() - base, freq);
                match &ev {
                    Event::Marker { text, .. } => println!("{ms:>10.3} ms  marker  {text}"),
                    Event::Dwm { id, keyword, .. } => {
                        let e = dwm_counts.entry(*id).or_insert((0, 0));
                        e.0 += 1;
                        e.1 |= keyword;
                    }
                }
            }
            Err(_) => continue,
        }
    }
    println!("Dwm-Core event counts by id (keywords 0x{DWM_KEYWORDS_DUMP:X}):");
    for (id, (n, kw)) in dwm_counts {
        println!("  id {id:>4}: {n:>6}  keyword 0x{kw:X}");
    }
    println!(
        "pin the composition-pass IDs for this build by passing them to the measurement runs, \
         e.g.: yspot-m0 toggle --dwm-ids 15,64"
    );
    Ok(())
}
