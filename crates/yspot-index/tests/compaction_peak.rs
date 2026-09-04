//! What compaction actually costs, measured rather than derived.
//!
//! The accel redesign (`docs/design/accel-redesign.md`, Step 9) states the
//! rule — *never build-and-swap, never allocate a second arena or a second FRN
//! table* — and asked for a gate: "assert with a counting allocator that peak
//! allocation never exceeds trigger + 6 MB/1M". That gate was never built, so
//! the §F memory table's `peak` column has only ever been arithmetic.
//!
//! It is arithmetic the Step 12 ruling turns on: §F puts Phase A's peak at
//! 102.4% of the 200 MB/1M cap at L = 34.6, which is the whole argument for
//! spending ~120 lines on a hand-rolled open-addressed FRN table. A derived
//! number carrying a ruling that size should be a measured one.
//!
//! So this file measures three things at a realistic mean folded-name length:
//! the steady state after a compaction, the state at the moment the trigger
//! fires, and the true allocation peak *during* `compact()`.
//!
//! **Everything runs in one test, on purpose.** The counters are global to the
//! process, and the harness runs `#[test]` functions on threads by default —
//! two measurements in flight at once would each be reading the other's
//! allocations. Splitting these into separate test functions is a mistake that
//! looks like a refactor.
//!
//! **What the allocator can and cannot see.** It counts bytes handed out by
//! the global allocator, which is what the process asks for — not RSS, which
//! also carries allocator slack and page granularity. A `realloc` that returns
//! the pointer it was given resized in place and is charged its delta; one
//! that returns a different pointer copied, and is charged both buffers for
//! the instant they coexisted. That distinction is the whole measurement here:
//! `shrink_to_fit` on a multi-megabyte arena either costs a transient copy of
//! it or costs nothing, and which one is not visible from the source.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use yspot_index::index::VolumeIndex;
use yspot_index::{EntrySink, UsnEvent};

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
/// Largest single allocation since the last reset, for naming the culprit
/// when the transient is bigger than it should be.
static BIGGEST: AtomicUsize = AtomicUsize::new(0);

struct Tracking;

fn grew(by: usize) {
    let now = LIVE.fetch_add(by, Ordering::Relaxed) + by;
    PEAK.fetch_max(now, Ordering::Relaxed);
    BIGGEST.fetch_max(by, Ordering::Relaxed);
}

// SAFETY: every method forwards to `System` unchanged; the counters are
// bookkeeping either side of the real call and never touch the returned block.
unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if p == ptr {
                // Resized where it stood: no second buffer ever existed, so
                // the only change is the delta. This is the case that decides
                // whether `shrink_to_fit` on a multi-megabyte arena costs a
                // transient copy of it or costs nothing at all, and the
                // returned pointer is the only way to tell from out here.
                if new_size >= layout.size() {
                    grew(new_size - layout.size());
                } else {
                    LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
                }
            } else {
                // Moved, so both buffers were live across the copy.
                grew(new_size);
                LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
            }
        }
        p
    }
}

#[global_allocator]
static ALLOC: Tracking = Tracking;

/// Names whose mean *folded* length is about `target_len`.
///
/// Deliberately varied rather than uniform: the arena packs them end to end,
/// so a fixed length would hide the rounding that real corpora pay, and the
/// whole point is to land on a realistic mean.
fn name_for(i: usize, target_len: usize) -> String {
    // A spread of roughly ±40% around the target, cycling so the mean lands
    // where it is asked to.
    const SPREAD: [i32; 8] = [-7, 4, -3, 9, -5, 2, 6, -6];
    let delta = SPREAD[i % SPREAD.len()];
    let len = (target_len as i32 + delta).max(4) as usize;
    let stem = format!("f{i:0>9}");
    let mut s = String::with_capacity(len);
    while s.len() + 4 < len {
        s.push_str(&stem[..(len - 4 - s.len()).min(stem.len())]);
    }
    s.push_str(".txt");
    s
}

struct Measured {
    steady: usize,
    trigger: usize,
    peak: usize,
    entries: usize,
    mean_folded: f64,
    reported: u64,
    biggest: usize,
    slots: usize,
}

fn per_1m(bytes: usize, entries: usize) -> f64 {
    bytes as f64 / entries as f64 * 1_000_000.0 / (1024.0 * 1024.0)
}

/// Fill, settle, churn to the trigger, compact — recording the allocator the
/// whole way.
fn measure(entries: usize, target_len: usize) -> Measured {
    let mut ix = VolumeIndex::new(0, "C:\\".to_string());
    for i in 0..entries {
        ix.add(i as u64 + 2, 1, &name_for(i, target_len), 0);
    }
    ix.finish();
    ix.finalize();

    // Churn until the index itself says it is time. Deletes free nothing on
    // their own — that is what makes compaction a thing — so this is the
    // honest way to reach the trigger state rather than assuming it.
    let mut next = entries as u64 + 2;
    let mut churned = 0usize;
    while !ix.should_compact() && churned < entries {
        let victim = churned as u64 + 2;
        ix.apply(UsnEvent::Delete { frn: victim });
        ix.apply(UsnEvent::Create {
            frn: next,
            parent_frn: 1,
            name: name_for(next as usize, target_len),
            flags: 0,
        });
        next += 1;
        churned += 1;
    }
    assert!(
        ix.should_compact(),
        "churned {churned} of {entries} without reaching the trigger"
    );

    let live_entries = ix.len();
    // Slots, not live entries: `remap` is sized by the pre-compaction slot
    // count, which is what the design doc's 4.50 MB/1M is derived from.
    let slots = live_entries + churned;
    let trigger = LIVE.load(Ordering::SeqCst);

    // Reset the high-water mark to now, so PEAK measures this call alone.
    PEAK.store(trigger, Ordering::SeqCst);
    BIGGEST.store(0, Ordering::SeqCst);
    ix.compact();
    let peak = PEAK.load(Ordering::SeqCst);
    let biggest = BIGGEST.load(Ordering::SeqCst);
    let steady = LIVE.load(Ordering::SeqCst);
    let reported = ix.ram_bytes();
    // Measured AFTER compaction: before it the arena still carries the stale
    // bytes the trigger fired on, and dividing those by the live count would
    // report an L a quarter higher than the corpus actually has.
    let mean_folded = ix.folded_arena_len() as f64 / live_entries.max(1) as f64;

    // Keep the index alive to here: dropping it early would return its bytes
    // before `steady` is read.
    drop(ix);
    Measured {
        steady,
        trigger,
        peak,
        entries: live_entries,
        mean_folded,
        reported,
        biggest,
        slots,
    }
}

/// The §F memory table at the size the cap is actually written for.
///
/// §10's budget is *per 1M entries*, and several structures here do not scale
/// with entry count at all — `tri_present` is a flat 2 MiB once the arena is
/// big enough to want it, and `frn_map`'s buckets step in powers of two. Divide
/// a 120k run by 120k and those fixed costs come out eight times too large, so
/// the absolute columns of a small run are not the budget and must not be read
/// as it. Only the transient extrapolates honestly, because `remap` really is
/// 4 B per slot.
///
/// Hence two tests: the fast one below guards the transient invariant on every
/// `cargo test`, and this one produces the numbers the §F table and the Step 12
/// ruling are actually about. `#[ignore]`d for runtime, like the rest of this
/// repo's slow and side-effecting tests; CI runs it with `--ignored`.
#[test]
#[ignore = "builds and churns 1M entries; CI runs it with --ignored"]
fn the_memory_budget_at_one_million_entries() {
    const ENTRIES: usize = 1_000_000;

    println!(
        "
     L   steady   trigger     peak   transient   ram_bytes()   (MB per 1M entries)"
    );
    for target in [25usize, 30, 35] {
        let m = measure(ENTRIES, target);
        let peak = per_1m(m.peak, m.entries);
        println!(
            "  {:>4.1}   {:>6.1}   {:>7.1}   {:>6.1}   {:>9.2}   {:>11.1}   {}",
            m.mean_folded,
            per_1m(m.steady, m.entries),
            per_1m(m.trigger, m.entries),
            peak,
            per_1m(m.peak.saturating_sub(m.trigger), m.entries),
            per_1m(m.reported as usize, m.entries),
            if peak > 200.0 { "OVER THE CAP" } else { "" },
        );
    }
}

/// The §F memory table, measured.
///
/// Three claims in one pass, because they share a measurement and the counters
/// are process-global (see the module header):
///
/// 1. **Step 9's gate.** The compaction transient is the `remap` array and
///    nothing else — the design doc puts it at 4.50 MB/1M and allows 6. A
///    build-and-swap, a second arena or a rebuilt FRN table would each blow
///    straight through that, which is the point: this is the tripwire for
///    someone reintroducing one.
/// 2. **Compaction gives bytes back**, or the trigger is just a stall.
/// 3. **`ram_bytes()` tells the truth.** It is what the service logs and what
///    §10's memory gate is read from, so a model that has drifted from what
///    was actually allocated would take the gate with it.
#[test]
fn compaction_never_allocates_a_second_index() {
    // Small on purpose: this guards the transient, which is linear in slots,
    // and is meant to run on every `cargo test`. The absolute columns it
    // prints are inflated by the fixed-size structures — see the test above
    // for the numbers that mean something.
    const ENTRIES: usize = 120_000;
    const BUDGET_MB_PER_1M: f64 = 6.0;

    println!(
        "
     L   steady   trigger     peak   transient   ram_bytes()   (MB per 1M entries)"
    );
    for target in [25usize, 30, 35] {
        let m = measure(ENTRIES, target);
        let transient = m.peak.saturating_sub(m.trigger);
        let transient_per_1m = per_1m(transient, m.entries);

        println!(
            "  {:>4.1}   {:>6.1}   {:>7.1}   {:>6.1}   {:>9.2}   {:>11.1}   biggest single alloc {:.2} MB (remap would be {:.2}, a second arena_recs {:.2})",
            m.mean_folded,
            per_1m(m.steady, m.entries),
            per_1m(m.trigger, m.entries),
            per_1m(m.peak, m.entries),
            transient_per_1m,
            per_1m(m.reported as usize, m.entries),
            m.biggest as f64 / 1048576.0,
            (m.slots * 4) as f64 / 1048576.0,
            (m.entries * 8) as f64 / 1048576.0,
        );

        assert!(
            transient_per_1m <= BUDGET_MB_PER_1M,
            "compaction transient {transient_per_1m:.2} MB/1M exceeds the {BUDGET_MB_PER_1M}              MB/1M budget at L={:.1} — someone reintroduced a build-and-swap, a second arena,              or a rebuilt FRN table (docs/design/accel-redesign.md, Step 9)",
            m.mean_folded
        );
        assert!(
            m.steady < m.trigger,
            "compaction freed nothing at L={:.1}: {} -> {}",
            m.mean_folded,
            m.trigger,
            m.steady
        );

        // Not an equality: the allocator sees the whole process — the harness,
        // the corpus strings, allocator slack the model does not model — while
        // `ram_bytes()` accounts for the index's own structures. The claim is
        // that it neither wildly over- nor under-states.
        let ratio = m.reported as f64 / m.steady as f64;
        assert!(
            (0.75..=1.25).contains(&ratio),
            "ram_bytes() is {ratio:.2}x what was allocated at L={:.1} — §10's memory gate              reads this number",
            m.mean_folded
        );
    }
}
