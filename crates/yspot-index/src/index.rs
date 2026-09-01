//! In-memory per-volume filename index (SPEC §3.4).
//!
//! Layout: one fixed-size [`Entry`] per file/dir; original-case names (UTF-8,
//! NFC-normalized) in one contiguous arena plus a case-folded shadow arena for
//! matching; an FRN → entry-index map for USN application and path
//! reconstruction. Full paths are never stored — [`VolumeIndex::path_of`]
//! walks the `parent_frn` chain to the volume root on demand.
//!
//! Mutations (`add`/`apply`) only append to the arenas; the bytes a deleted or
//! renamed entry leaves behind are erased in place but not reclaimed until the
//! next full rebuild/snapshot cycle (§3.7). This keeps USN application O(1)
//! and is bounded by rebuild cadence.
//!
//! Folded-arena layout: `0x00 rec 0x00 rec 0x00`. Every record is fenced by
//! [`FOLDED_DELIM`], no record may contain a byte below `0x20`, and a vacated
//! record is overwritten with delimiters. That is what lets the matcher read a
//! hit's tier straight off `arena[hit - 1]` / `arena[hit + qlen]`, and what
//! makes a match spanning two records structurally impossible rather than
//! something the query path has to detect and discard.
//!
//! Slot stability: `entries[i]` belongs to one file for its lifetime. A delete
//! tombstones the slot ([`crate::flags::DEAD`]) and offers it to `free_slots`;
//! it never moves another entry. That is what allows structures to be keyed by
//! entry index, and it is why `len()` is a maintained live count rather than
//! `entries.len()`.
//!
//! [`VolumeIndex::rank_key`] is the first such structure: one byte per slot
//! holding everything §3.4's score needs about an entry other than its match
//! quality. It is maintained incrementally at the two mutation choke points,
//! settled by a memoized sweep in [`VolumeIndex::finalize`], and repaired on
//! the WRITER after a directory reparent — never rebuilt from a query.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use unicode_normalization::UnicodeNormalization;

use crate::matching::Accel;
use crate::{Hit, UsnEvent};

/// Cycle/depth guard for parent-chain walks. NTFS practical depth is far
/// below this; the cap only defends against corrupt/cyclic parent chains.
pub(crate) const PATH_DEPTH_CAP: u32 = 512;

/// Widest depth [`VolumeIndex::rank_key`] can carry: it packs depth into 7
/// bits so one byte also holds the HIDDEN/SYSTEM flag.
///
/// This is a RANKING clamp only. [`VolumeIndex::depth_of`] and
/// [`VolumeIndex::path_of`] keep [`PATH_DEPTH_CAP`] semantics untouched, and
/// the penalty difference between depth 127 (1/3.54) and 512 (1/11.2) is only
/// reachable on a corrupt or cyclic parent chain.
pub(crate) const RANK_DEPTH_MAX: u8 = 127;

/// `rank_key` bit 7: the entry carries HIDDEN or SYSTEM (§3.4's ×0.85).
const RANK_PENALIZED: u8 = 0x80;

/// `depth_penalty × hidden_penalty` for every [`VolumeIndex::rank_key`] value,
/// so the §3.4 score is `base × DEPTH_PEN[rank_key[slot]]`: one L2 read into a
/// 1 B/entry column, one L1 table lookup and one multiply.
///
/// What it replaces is the whole reason this table exists: the ranking loop
/// used to call `depth_of` per candidate, a parent-chain walk with a `frn_map`
/// probe per level into a 20-35 MB map (~250-400 ns each). At tens of
/// thousands of candidates that walk *was* the query.
///
/// Indexed by the packed key, so both factors are folded in: the low 7 bits
/// give `1 / (1 + 0.02·depth)` and bit 7 multiplies by 0.85.
pub(crate) static DEPTH_PEN: [f32; 256] = {
    let mut t = [0.0f32; 256];
    let mut k = 0usize;
    while k < 256 {
        let depth = (k & RANK_DEPTH_MAX as usize) as f32;
        let p = 1.0 / (1.0 + 0.02 * depth);
        t[k] = if k & RANK_PENALIZED as usize != 0 {
            p * 0.85
        } else {
            p
        };
        k += 1;
    }
    t
};

/// Depth-sweep marks. One byte per slot, live only for the duration of a
/// sweep: `UNKNOWN` → not yet visited, `ON_STACK` → on the current upward
/// walk (so meeting it again is a cycle), `DONE` → its `rank_key` depth is
/// authoritative and can be used as a memo base.
const D_UNKNOWN: u8 = 0;
const D_ON_STACK: u8 = 1;
const D_DONE: u8 = 2;

/// How long a directory reparent waits before the writer repairs its
/// descendants' cached depths (design §5, behavior change 4). Long enough that
/// a `move` of a large tree coalesces into one sweep.
pub const DEPTH_REPAIR_DEBOUNCE: Duration = Duration::from_millis(500);

/// Slots repaired per [`VolumeIndex::repair_depths_slice`] call, i.e. per
/// write-lock acquisition. The sweep is memoized and O(1) amortized per slot,
/// so this bounds the stall rather than the total work.
const DEPTH_REPAIR_SLICE: usize = 64 * 1024;

/// Post-[`VolumeIndex::finalize`] growth steps: 64 Ki entries (2 MiB) and
/// 1 MiB per arena, taken with `reserve_exact`.
///
/// `Vec`/`String` doubling would re-acquire, on the very next USN event, the
/// slack `finalize` just released — up to ~2x on a structure that is tens of
/// MB at 1M entries. Fixed chunks bound a settled index's allocation
/// overshoot at ~4 MiB total (2 MiB entries + 2 × 1 MiB arenas) regardless of
/// its size, which is the 4.19 B/entry line of the §3.4 memory budget.
const ENTRY_GROW_CHUNK: usize = 64 * 1024;
const ARENA_GROW_CHUNK: usize = 1024 * 1024;

/// Folded-arena record delimiter. U+0000 is valid UTF-8 and, by
/// [`has_control_byte`], can never occur inside a record — so one byte fences
/// each record with no escaping and no ambiguity, and a delimiter-free query
/// cannot match across two records. Costs 1 B/record.
pub(crate) const FOLDED_DELIM: u8 = 0x00;

/// Whether folded text carries a byte the folded arena reserves for itself.
///
/// The arena's whole design rests on nothing below `0x20` appearing inside a
/// record, so the rule is enforced on both sides of the matcher from this one
/// definition: [`VolumeIndex::intern`] drops such names, and
/// [`crate::matching::search`] rejects such queries. NTFS forbids these bytes
/// in filenames, so only synthetic and walk-mode inputs can produce them.
pub(crate) fn has_control_byte(s: &str) -> bool {
    s.bytes().any(|b| b < 0x20)
}

/// One file or directory (§3.4). 32 B with padding.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Entry {
    pub frn: u64,
    pub parent_frn: u64,
    pub name_off: u32,
    pub folded_off: u32,
    pub name_len: u16,
    pub folded_len: u16,
    pub flags: u16,
}

impl Entry {
    /// Tombstoned slot: the file is deleted (or the slot's name was replaced
    /// by a rename) but the slot is retained. Its arena bytes are still in
    /// place and still scannable, so every candidate-generating pass has to
    /// filter dead slots out or it will return a file that no longer exists.
    pub(crate) fn is_dead(&self) -> bool {
        self.flags & crate::flags::DEAD != 0
    }
}

/// The single case fold used everywhere (§3.4): NFC-normalize, then char-wise
/// `to_lowercase`. Applied identically to arena names and incoming queries so
/// NFC/NFD spellings and case variants of the same name match. M1 swaps in
/// ICU4X simple case folding behind this same function.
pub fn fold(s: &str) -> String {
    s.nfc().flat_map(|c| c.to_lowercase()).collect()
}

/// Truncate to at most `max` bytes on a char boundary (defensive; NTFS names
/// are ≤ 255 UTF-16 units, far below the u16 length fields).
fn truncate_to_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Approximate resident bytes per index structure, so
/// `IndexStatus.ram_bytes.filename` (§4.3) is attributable to a structure
/// rather than being one opaque number.
///
/// Every field is *capacity*, not payload: allocator overshoot is exactly
/// what the §3.4 memory budget is spent on, so it has to be visible.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RamBreakdown {
    /// Entry table: capacity × 32 B.
    pub entries: u64,
    /// Original-case name arena capacity.
    pub name_arena: u64,
    /// Case-folded shadow arena capacity.
    pub folded_arena: u64,
    /// FRN → index map, charged per hashbrown bucket — see
    /// [`VolumeIndex::ram_breakdown`] for the arithmetic.
    pub frn_map: u64,
    /// Tombstoned slots awaiting reuse; grows with delete churn and drains
    /// back to zero as creates recycle them.
    pub free_slots: u64,
    /// Packed depth + hidden/system ranking column, 1 B per slot.
    pub rank_key: u64,
    /// Matcher accel: sorted `(folded_off, entry_idx)` vec.
    pub folded_order: u64,
    /// Matcher accel: initials arena plus its span table.
    pub initials: u64,
    /// Matcher accel: byte-trigram postings; zero until the first fuzzy query.
    pub trigrams: u64,
}

impl RamBreakdown {
    /// What [`VolumeIndex::ram_bytes`] returns.
    pub fn total(&self) -> u64 {
        self.entries
            + self.name_arena
            + self.folded_arena
            + self.frn_map
            + self.free_slots
            + self.rank_key
            + self.folded_order
            + self.initials
            + self.trigrams
    }
}

/// In-memory index of one NTFS volume (§3.4).
pub struct VolumeIndex {
    volume_idx: u32,
    root_path: String,
    pub(crate) entries: Vec<Entry>,
    /// Original-case names, UTF-8, NFC-normalized, back to back.
    pub(crate) name_arena: String,
    /// Case-folded shadow of `name_arena` (offsets differ; folding expands),
    /// laid out as `0x00 rec 0x00 rec 0x00` — see [`FOLDED_DELIM`]. This is
    /// the arena the matcher scans, so it is the one that has to be fenced.
    pub(crate) folded_arena: String,
    /// FRN → index into `entries`.
    pub(crate) frn_map: HashMap<u64, u32>,
    /// Entries not carrying [`crate::flags::DEAD`]; equivalently
    /// `frn_map.len()`, maintained rather than derived because it is what
    /// [`Self::len`] answers on the hot status path.
    live_count: usize,
    /// Tombstoned slots, newest first, handed back to the next create so the
    /// entry table does not grow under delete/create churn.
    free_slots: Vec<u32>,
    /// Name + folded arena bytes belonging to tombstoned or renamed-away
    /// names. The compaction trigger (§3.7) is a fraction of this.
    dead_bytes: usize,
    /// Everything ranking needs about a slot, in one byte:
    /// `depth.min(127) | (hidden|system) << 7`. Indexed by slot, so it is
    /// exactly as long as `entries`.
    ///
    /// Deliberately NOT a field of [`Entry`]: the ranking loop touches one
    /// candidate at a time in arena order, and a 1 MB column streams where a
    /// 32 MB entry table misses. Scoring reads it through [`DEPTH_PEN`].
    pub(crate) rank_key: Vec<u8>,
    /// When a directory reparent armed the writer-side depth repair, or `None`
    /// if no repair is outstanding. See [`Self::repair_depths_slice`].
    depth_repair_armed: Option<Instant>,
    /// Next slot the outstanding repair will visit.
    depth_repair_cursor: usize,
    /// Sweep marks for the outstanding repair, carried across slices so the
    /// sliced sweep stays O(n) in total rather than O(n·depth). Empty
    /// whenever no repair is running.
    depth_repair_state: Vec<u8>,
    /// Lazily rebuilt matcher acceleration structures (§3.4 prefilters).
    /// Mutex (not RefCell) so `search(&self)` stays `Sync`-safe behind an
    /// outer `RwLock` in the service.
    pub(crate) accel: Mutex<Accel>,
    /// Set by [`VolumeIndex::finalize`]: from then on the entry table and the
    /// arenas grow in fixed [`ENTRY_GROW_CHUNK`]/[`ARENA_GROW_CHUNK`] steps
    /// instead of doubling.
    settled: bool,
    /// High-water bucket count of `frn_map`, maintained on insert and reset
    /// only in [`VolumeIndex::finalize`]. See [`VolumeIndex::ram_breakdown`].
    frn_map_buckets: usize,
}

impl VolumeIndex {
    /// `root_path` is the mount anchor for path reconstruction, e.g. `C:\`.
    pub fn new(volume_idx: u32, root_path: String) -> Self {
        Self {
            volume_idx,
            root_path,
            entries: Vec::new(),
            name_arena: String::new(),
            folded_arena: String::new(),
            frn_map: HashMap::new(),
            live_count: 0,
            free_slots: Vec::new(),
            dead_bytes: 0,
            rank_key: Vec::new(),
            depth_repair_armed: None,
            depth_repair_cursor: 0,
            depth_repair_state: Vec::new(),
            accel: Mutex::new(Accel::new()),
            settled: false,
            frn_map_buckets: 0,
        }
    }

    pub fn volume_idx(&self) -> u32 {
        self.volume_idx
    }

    pub fn root_path(&self) -> &str {
        &self.root_path
    }

    /// Live entries. NOT `entries.len()`: deletes tombstone in place, so the
    /// entry table also holds slots waiting to be recycled.
    pub fn len(&self) -> usize {
        self.live_count
    }

    pub fn is_empty(&self) -> bool {
        self.live_count == 0
    }

    /// Arena bytes (original + folded) owned by tombstoned or renamed-away
    /// names. Instrumentation for the §3.7 compaction trigger.
    pub fn dead_bytes(&self) -> usize {
        self.dead_bytes
    }

    /// Live entries paired with their slot index, dead slots skipped.
    ///
    /// Every full sweep of the entry table must go through this: a tombstone
    /// keeps its name in both arenas, so a sweep that misses one indexes a
    /// deleted file and returns it as a hit — wrong results, not a crash.
    pub(crate) fn live_entries(&self) -> impl Iterator<Item = (u32, &Entry)> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.is_dead())
            .map(|(i, e)| (i as u32, e))
    }

    /// Original-case (NFC) name of an indexed FRN, if present.
    pub fn name_of(&self, frn: u64) -> Option<&str> {
        let &idx = self.frn_map.get(&frn)?;
        Some(self.name_of_entry(&self.entries[idx as usize]))
    }

    pub(crate) fn name_of_entry(&self, e: &Entry) -> &str {
        &self.name_arena[e.name_off as usize..e.name_off as usize + e.name_len as usize]
    }

    pub(crate) fn folded_of_entry(&self, e: &Entry) -> &str {
        &self.folded_arena[e.folded_off as usize..e.folded_off as usize + e.folded_len as usize]
    }

    /// Poison-tolerant lock of the matcher acceleration structures.
    pub(crate) fn accel_lock(&self) -> MutexGuard<'_, Accel> {
        self.accel.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Release bulk-load growth slack (§3.4 memory budget) and switch to
    /// chunked growth. Called once through [`crate::EntrySink::finish`] when
    /// enumeration (§3.2) or the dev walk has pushed its last entry.
    ///
    /// Doubling leaves each structure holding up to ~2x what it needs, which
    /// at 1M entries is tens of MB of nothing; `shrink_to_fit` hands it back.
    /// `settled` then keeps it handed back: without it the next USN event
    /// would re-double an arena straight back to where it was.
    /// Release bulk-load growth slack and settle the cached ranking depths.
    pub fn finalize(&mut self) {
        self.entries.shrink_to_fit();
        self.name_arena.shrink_to_fit();
        self.folded_arena.shrink_to_fit();
        self.frn_map.shrink_to_fit();
        self.rank_key.shrink_to_fit();
        // The one place the map's allocation actually gets smaller, so the one
        // place the monotonic bucket count may be lowered.
        self.frn_map_buckets = 0;
        self.note_frn_map_buckets();
        self.settled = true;
        self.sweep_all_depths();
    }

    /// Recompute every slot's cached ranking depth in one memoized sweep.
    ///
    /// Required, not an optimization: `FSCTL_ENUM_USN_DATA` walks in MFT-record
    /// order, not parent-first, so a child enumerated before its parent gets
    /// depth 0 from [`Self::register_slot`] — it would then outrank the file it
    /// is nested twenty levels below. Steady-state USN creates almost always
    /// arrive after their parent, which is why the incremental rule is enough
    /// between sweeps.
    fn sweep_all_depths(&mut self) {
        let n = self.entries.len();
        let mut state = vec![D_UNKNOWN; n];
        let mut stack: Vec<u32> = Vec::new();
        self.depth_sweep(0, n, &mut state, &mut stack);
        // A full sweep supersedes any outstanding sliced repair.
        self.depth_repair_armed = None;
        self.depth_repair_cursor = 0;
        self.depth_repair_state = Vec::new();
    }

    /// Memoized, cycle-guarded depth sweep over slots `[start, end)`.
    ///
    /// Each slot walks up its `parent_frn` chain until it meets a slot already
    /// marked `D_DONE` (whose cached depth is the memo base), the volume root,
    /// a missing parent, or itself — then the walk is unwound and every slot on
    /// it is assigned and marked. Every slot is therefore pushed and assigned
    /// exactly once across a whole sweep: O(n), not O(n·depth).
    ///
    /// A chain that closes on itself gets [`RANK_DEPTH_MAX`] throughout, which
    /// is what [`Self::depth_of`] reports for a cycle once clamped
    /// (`PATH_DEPTH_CAP` ≥ `RANK_DEPTH_MAX`), so the cached value and the
    /// walking one agree on corrupt chains too.
    fn depth_sweep(&mut self, start: usize, end: usize, state: &mut Vec<u8>, stack: &mut Vec<u32>) {
        debug_assert_eq!(self.rank_key.len(), self.entries.len());
        if state.len() < self.entries.len() {
            state.resize(self.entries.len(), D_UNKNOWN);
        }
        let end = end.min(self.entries.len());
        for s in start..end {
            if state[s] != D_UNKNOWN || self.entries[s].is_dead() {
                continue;
            }
            stack.clear();
            let mut cur = s as u32;
            // Depth to assign to the LAST slot pushed; the walk unwinds from
            // there back down to `s`, incrementing.
            let mut first = 0u32;
            let mut cycle = false;
            loop {
                match state[cur as usize] {
                    D_DONE => {
                        let d = (self.rank_key[cur as usize] & RANK_DEPTH_MAX) as u32;
                        first = (d + 1).min(RANK_DEPTH_MAX as u32);
                        break;
                    }
                    D_ON_STACK => {
                        cycle = true;
                        break;
                    }
                    _ => {}
                }
                state[cur as usize] = D_ON_STACK;
                stack.push(cur);
                let e = self.entries[cur as usize];
                if e.parent_frn == e.frn {
                    break; // volume root records itself as its own parent
                }
                match self.frn_map.get(&e.parent_frn) {
                    // `frn_map` never holds a tombstone, so a hit is live.
                    Some(&p) if p != cur => cur = p,
                    _ => break, // missing parent: anchored at the root
                }
            }
            if cycle {
                for &slot in stack.iter() {
                    self.set_rank_depth(slot, RANK_DEPTH_MAX as u32);
                    state[slot as usize] = D_DONE;
                }
            } else {
                let mut d = first;
                for &slot in stack.iter().rev() {
                    self.set_rank_depth(slot, d);
                    state[slot as usize] = D_DONE;
                    d = (d + 1).min(RANK_DEPTH_MAX as u32);
                }
            }
        }
    }

    /// Write `depth` into a slot's `rank_key`, keeping its hidden/system bit.
    fn set_rank_depth(&mut self, slot: u32, depth: u32) {
        let k = &mut self.rank_key[slot as usize];
        *k = (*k & RANK_PENALIZED) | (depth.min(RANK_DEPTH_MAX as u32) as u8);
    }

    /// Arm the writer-side depth repair: a directory changed parent, so every
    /// descendant's cached depth is now stale by a constant offset.
    ///
    /// Re-arming restarts the sweep from slot 0, which is what makes the
    /// carried-over marks sound: the only mutation that invalidates an already
    /// swept slot's depth is another reparent, and that lands here.
    fn arm_depth_repair(&mut self) {
        self.depth_repair_armed = Some(Instant::now());
        self.depth_repair_cursor = 0;
        self.depth_repair_state = Vec::new();
    }

    /// Whether a directory reparent is waiting and its
    /// [`DEPTH_REPAIR_DEBOUNCE`] has elapsed.
    ///
    /// Polled by the WRITER (the USN tail loop), never by `search`. Refreshing
    /// depth on the query path would re-arm exactly the rebuild-on-mutation
    /// cliff this column exists to remove, inside the §2.5 10 ms budget; the
    /// query path reads `rank_key` unconditionally, with no epoch check and no
    /// fallback walk. The price is bounded staleness on a moved subtree — a few
    /// percent of score for a few hundred ms, and no entry ever appears or
    /// disappears because of it (behavior change 4).
    pub fn depth_repair_due(&self) -> bool {
        matches!(self.depth_repair_armed, Some(t) if t.elapsed() >= DEPTH_REPAIR_DEBOUNCE)
    }

    /// Advance an armed depth repair by one [`DEPTH_REPAIR_SLICE`] of slots.
    /// Returns `true` while work remains, so the caller can release and retake
    /// the write lock between slices.
    pub fn repair_depths_slice(&mut self) -> bool {
        if self.depth_repair_armed.is_none() {
            return false;
        }
        let n = self.entries.len();
        if self.depth_repair_state.len() < n {
            // Slots appended since the repair started sit past the cursor, so
            // they are swept in their turn; recycled slots below it already
            // carry a fresh depth from `register_slot`.
            self.depth_repair_state.resize(n, D_UNKNOWN);
        }
        let start = self.depth_repair_cursor.min(n);
        let end = (start + DEPTH_REPAIR_SLICE).min(n);
        let mut state = std::mem::take(&mut self.depth_repair_state);
        let mut stack: Vec<u32> = Vec::new();
        self.depth_sweep(start, end, &mut state, &mut stack);
        self.depth_repair_state = state;
        self.depth_repair_cursor = end;
        if end < n {
            return true;
        }
        self.depth_repair_armed = None;
        self.depth_repair_cursor = 0;
        self.depth_repair_state = Vec::new();
        false
    }

    /// Make room for `need` more arena bytes in whole [`ARENA_GROW_CHUNK`]s.
    /// `reserve_exact` counts from `len`, so the current spare is asked for
    /// again — otherwise the reservation would shrink the arena's headroom.
    fn reserve_arena(arena: &mut String, need: usize) {
        let spare = arena.capacity() - arena.len();
        if spare >= need {
            return;
        }
        let chunks = (need - spare).div_ceil(ARENA_GROW_CHUNK);
        arena.reserve_exact(spare + chunks * ARENA_GROW_CHUNK);
    }

    fn mark_dirty(&mut self) {
        self.accel
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .dirty = true;
    }

    /// NFC-normalize + fold `name` and append both forms to the arenas.
    /// Returns `(name_off, name_len, folded_off, folded_len)`, or `None` for
    /// unusable names (empty after normalization, a folded form carrying a
    /// byte the arena reserves, or arena offsets exhausted).
    ///
    /// Appends the folded record as `rec 0x00`, laying down the opening
    /// `0x00` on the first call. The trailing delimiter of one record is the
    /// leading delimiter of the next, so the arena is `0x00 rec 0x00 rec 0x00`
    /// at one byte per record — and both `arena[folded_off - 1]` and
    /// `arena[folded_off + folded_len]` are unconditionally in bounds and
    /// equal to [`FOLDED_DELIM`], which is what the matcher's tier test reads.
    ///
    /// Nothing is written to either arena until every rejection test has
    /// passed: a half-interned name would leave orphan bytes that the next
    /// record's offsets sit after and that no entry claims.
    fn intern(&mut self, name: &str) -> Option<(u32, u16, u32, u16)> {
        let nfc: String = name.nfc().collect();
        let nfc = truncate_to_boundary(&nfc, u16::MAX as usize);
        if nfc.is_empty() {
            return None;
        }
        let folded_full = fold(nfc);
        let folded = truncate_to_boundary(&folded_full, u16::MAX as usize);
        // A control byte would either forge a delimiter — splitting one name
        // into two records that no offset accounts for — or leave the tier
        // test reading a fence that is not one. Dropped like the empty name.
        if folded.is_empty() || has_control_byte(folded) {
            log::debug!("index: name with a reserved byte in its folded form dropped");
            return None;
        }
        // This record's trailing delimiter, plus the opening one if the arena
        // has not been written to yet.
        let fences = 1 + usize::from(self.folded_arena.is_empty());
        if self.name_arena.len() + nfc.len() > u32::MAX as usize
            || self.folded_arena.len() + folded.len() + fences > u32::MAX as usize
        {
            log::error!("index: name arena offset space exhausted; entry dropped");
            return None;
        }
        if self.settled {
            Self::reserve_arena(&mut self.name_arena, nfc.len());
            Self::reserve_arena(&mut self.folded_arena, folded.len() + fences);
        }
        let name_off = self.name_arena.len() as u32;
        self.name_arena.push_str(nfc);
        if self.folded_arena.is_empty() {
            self.folded_arena.push(FOLDED_DELIM as char);
        }
        let folded_off = self.folded_arena.len() as u32;
        self.folded_arena.push_str(folded);
        self.folded_arena.push(FOLDED_DELIM as char);
        Some((name_off, nfc.len() as u16, folded_off, folded.len() as u16))
    }

    /// Bind `slot` to a live entry: write the [`Entry`], publish it in
    /// `frn_map`, and count it live. `interned` is what [`Self::intern`]
    /// returned for the entry's name.
    ///
    /// One of the two places any per-slot structure is populated (the other is
    /// [`Self::unregister_slot`]). Create, the same-FRN re-create and rename
    /// all route through here, so a column added later has exactly two hooks
    /// and cannot be left stale by a mutation path nobody remembered.
    fn register_slot(
        &mut self,
        slot: u32,
        frn: u64,
        parent_frn: u64,
        interned: (u32, u16, u32, u16),
        flags: u16,
    ) {
        let (name_off, name_len, folded_off, folded_len) = interned;
        // DEAD is ours, never a filesystem attribute. `EntrySink` is public, so
        // an out-of-crate caller passing bit 3 would mint an entry that
        // `frn_map`, `name_of` and `path_of` resolve but `live_entries` skips
        // forever — permanently unsearchable, with no release-mode signal.
        // Mask it rather than only asserting on it.
        debug_assert_eq!(flags & crate::flags::DEAD, 0, "DEAD is not an attribute");
        let flags = flags & !crate::flags::DEAD;
        debug_assert!(slot as usize <= self.entries.len(), "slot out of range");
        // Ranking key, computed BEFORE `frn_map` learns about this FRN so a
        // self-parenting root cannot read its own half-written depth. The
        // parent's cached depth is the whole walk: O(1), and correct whenever
        // the parent is already indexed, which is the steady-state USN case.
        // Bulk load can enumerate a child first — that is what `finalize`'s
        // sweep is for — and a directory reparent is repaired by
        // `repair_depths_slice`.
        let depth = if parent_frn == frn {
            0
        } else {
            match self.frn_map.get(&parent_frn) {
                Some(&p) if p != slot => {
                    ((self.rank_key[p as usize] & RANK_DEPTH_MAX) + 1).min(RANK_DEPTH_MAX)
                }
                _ => 0,
            }
        };
        let penalized = u8::from(flags & (crate::flags::HIDDEN | crate::flags::SYSTEM) != 0);
        let key = depth | (penalized * RANK_PENALIZED);
        let e = Entry {
            frn,
            parent_frn,
            name_off,
            folded_off,
            name_len,
            folded_len,
            flags,
        };
        if slot as usize == self.entries.len() {
            // Each column is guarded on its OWN capacity. Sharing one test
            // works only while the two capacities move in lockstep, and the
            // moment they diverge the unguarded one silently falls back to
            // geometric doubling — the behavior Step 1 exists to remove. Every
            // column added later needs this same shape.
            if self.settled {
                if self.entries.len() == self.entries.capacity() {
                    self.entries.reserve_exact(ENTRY_GROW_CHUNK);
                }
                if self.rank_key.len() == self.rank_key.capacity() {
                    self.rank_key.reserve_exact(ENTRY_GROW_CHUNK);
                }
            }
            self.entries.push(e);
            self.rank_key.push(key);
        } else {
            self.entries[slot as usize] = e;
            self.rank_key[slot as usize] = key;
        }
        debug_assert_eq!(self.rank_key.len(), self.entries.len());
        self.frn_map.insert(frn, slot);
        self.live_count += 1;
        self.note_frn_map_buckets();
        debug_assert_eq!(self.live_count, self.frn_map.len());
    }

    /// Record the map's bucket count after a possible grow. Monotonic by
    /// construction: see [`Self::ram_breakdown`] for why re-deriving it from
    /// `capacity()` after deletions under-reports.
    fn note_frn_map_buckets(&mut self) {
        let cap = self.frn_map.capacity();
        let buckets = if cap == 0 {
            0 // a never-inserted map has not allocated at all
        } else {
            (cap * 8 / 7).next_power_of_two().max(4) // hashbrown's own floor
        };
        self.frn_map_buckets = self.frn_map_buckets.max(buckets);
    }

    /// Tombstone `slot`: drop it from `frn_map`, mark it [`crate::flags::DEAD`],
    /// erase its folded record and charge its arena bytes to `dead_bytes`. The
    /// entry stays where it is — nothing is ever swap-removed, because that
    /// would permute every other entry's index.
    ///
    /// The freelist push is deliberately the CALLER's: a rename tears its slot
    /// down and re-registers the same slot, and that slot must never appear in
    /// `free_slots` while it is live.
    fn unregister_slot(&mut self, slot: u32) {
        let e = &mut self.entries[slot as usize];
        debug_assert!(!e.is_dead(), "slot {slot} unregistered twice");
        e.flags |= crate::flags::DEAD;
        let frn = e.frn;
        let bytes = e.name_len as usize + e.folded_len as usize;
        let (folded_off, folded_len) = (e.folded_off as usize, e.folded_len as usize);
        self.frn_map.remove(&frn);
        // A tombstone gets the WORST ranking key, not zero. The query path
        // reads `rank_key` with no liveness check by design, so if a dead slot
        // ever does leak through a pass, it must sort to the bottom of the page
        // rather than the top — and zero is the most favourable byte the column
        // can hold (depth 0, unpenalized, DEPTH_PEN 1.0). Safe to poison
        // because a new occupant's depth is derived through `frn_map`, which
        // never holds a tombstone.
        self.rank_key[slot as usize] = RANK_DEPTH_MAX | RANK_PENALIZED;
        self.erase_folded(folded_off, folded_len);
        self.dead_bytes += bytes;
        self.live_count -= 1;
        debug_assert_eq!(self.live_count, self.frn_map.len());
    }

    /// Overwrite a vacated folded record with [`FOLDED_DELIM`] (invariant I4).
    ///
    /// Those bytes used to be left in place and filtered out on the query path
    /// by a bounds check against the live records' offsets — dead bytes could
    /// still produce a hit, and every pass had to know to throw it away.
    /// Erasing makes them *inert*: a delimiter-free query cannot match inside
    /// a run of delimiters at all. The erased record merges with its two
    /// fences into one longer run, which is still a well-formed arena, because
    /// records are addressed by their stored `folded_off` and never by
    /// counting delimiters.
    ///
    /// The name arena is deliberately left alone: it is never scanned, only
    /// indexed through a live entry, and `path_of` reads it on the hot path.
    fn erase_folded(&mut self, off: usize, len: usize) {
        // SAFETY: `off..off + len` is exactly one interned record, so both
        // ends are char boundaries; U+0000 is a one-byte encoding, so
        // replacing whole chars with it leaves the arena valid UTF-8.
        let bytes = unsafe { self.folded_arena.as_mut_vec() };
        bytes[off..off + len].fill(FOLDED_DELIM);
    }

    /// Add one entry; an existing FRN is updated in place (new name appended
    /// to the arenas, old folded record erased where it stands and its bytes
    /// charged as dead — the space itself is not reclaimed until rebuild,
    /// §3.7).
    pub(crate) fn add_entry(&mut self, frn: u64, parent_frn: u64, name: &str, flags: u16) {
        let existing = self.frn_map.get(&frn).copied();
        // Only a create that has to append a slot can exhaust the index space;
        // recycling a tombstone never does.
        if existing.is_none()
            && self.free_slots.is_empty()
            && self.entries.len() >= u32::MAX as usize
        {
            log::error!("index: entry table full; frn {frn:#x} dropped");
            return;
        }
        let Some(interned) = self.intern(name) else {
            log::warn!("index: skipping unusable name for frn {frn:#x}");
            return;
        };
        let slot = match existing {
            // A Create for an FRN we already hold is NTFS reusing the record:
            // it behaves as a rename, so tear the old name down through the
            // same choke point a rename uses and re-register the same slot.
            Some(idx) => {
                self.note_reparent(idx, parent_frn);
                self.unregister_slot(idx);
                idx
            }
            None => self.free_slots.pop().unwrap_or(self.entries.len() as u32),
        };
        self.register_slot(slot, frn, parent_frn, interned, flags);
        self.mark_dirty();
    }

    /// Arm the depth repair if `slot` is a directory that is about to change
    /// parent: its descendants' cached depths all shift by a constant.
    ///
    /// A non-directory has no descendants and `register_slot` recomputes its
    /// own depth, so a file move costs nothing.
    fn note_reparent(&mut self, slot: u32, new_parent_frn: u64) {
        let e = &self.entries[slot as usize];
        if e.flags & crate::flags::DIR != 0 && e.parent_frn != new_parent_frn {
            self.arm_depth_repair();
        }
    }

    fn remove_entry(&mut self, frn: u64) {
        let Some(&idx) = self.frn_map.get(&frn) else {
            return;
        };
        // Deleting a directory changes every surviving descendant's true depth
        // — their chain now breaks at the missing parent — but their cached
        // `rank_key` keeps the old, larger value. Without arming the repair
        // that staleness is UNBOUNDED, and behavior change 4 only signs off
        // bounded staleness. Costs one timestamp write; the sweep is memoized
        // and sliced. Ranking only, never membership.
        if self.entries[idx as usize].flags & crate::flags::DIR != 0 {
            self.arm_depth_repair();
        }
        self.unregister_slot(idx);
        // The slot keeps its place in the table and its (now dead) arena bytes
        // until rebuild (§3.7); the next create takes it back.
        self.free_slots.push(idx);
        self.mark_dirty();
    }

    fn rename_entry(&mut self, frn: u64, new_parent_frn: u64, new_name: &str) {
        let Some(&idx) = self.frn_map.get(&frn) else {
            log::debug!("index: rename for unknown frn {frn:#x} ignored");
            return;
        };
        let Some(interned) = self.intern(new_name) else {
            log::warn!("index: rename to unusable name for frn {frn:#x} ignored");
            return;
        };
        // Read before tombstoning: a rename carries no attributes, so the
        // entry's own flags are what get re-registered.
        let flags = self.entries[idx as usize].flags;
        self.note_reparent(idx, new_parent_frn);
        self.unregister_slot(idx);
        self.register_slot(idx, frn, new_parent_frn, interned, flags);
        self.mark_dirty();
    }

    /// Apply one decoded USN journal event (§3.3).
    pub fn apply(&mut self, ev: UsnEvent) {
        match ev {
            UsnEvent::Create {
                frn,
                parent_frn,
                name,
                flags,
            } => self.add_entry(frn, parent_frn, &name, flags),
            UsnEvent::Delete { frn } => self.remove_entry(frn),
            UsnEvent::Rename {
                frn,
                new_parent_frn,
                new_name,
            } => self.rename_entry(frn, new_parent_frn, &new_name),
            // ACL-cache invalidation only (§3.8); no index change in M0.
            UsnEvent::SecurityChange { .. } => {}
        }
    }

    /// Reconstruct the absolute path of `frn` by walking the `parent_frn`
    /// chain (§3.4). A missing parent anchors the chain at `root_path`;
    /// cyclic/corrupt chains are cut at [`PATH_DEPTH_CAP`].
    pub fn path_of(&self, frn: u64) -> Option<String> {
        let mut idx = *self.frn_map.get(&frn)?;
        let mut parts: Vec<(u32, u16)> = Vec::new();
        for _ in 0..PATH_DEPTH_CAP {
            let e = &self.entries[idx as usize];
            parts.push((e.name_off, e.name_len));
            if e.parent_frn == e.frn {
                break; // volume root records itself as its own parent
            }
            match self.frn_map.get(&e.parent_frn) {
                Some(&p) if p != idx => idx = p,
                _ => break, // missing parent: anchor at root_path
            }
        }
        let mut out = String::with_capacity(
            self.root_path.len()
                + parts
                    .iter()
                    .map(|&(_, len)| len as usize + 1)
                    .sum::<usize>(),
        );
        out.push_str(&self.root_path);
        if !out.ends_with('\\') {
            out.push('\\');
        }
        for (i, &(off, len)) in parts.iter().rev().enumerate() {
            if i > 0 {
                out.push('\\');
            }
            out.push_str(&self.name_arena[off as usize..off as usize + len as usize]);
        }
        Some(out)
    }

    /// Number of parent links successfully followed from `entry_idx`, capped
    /// at [`PATH_DEPTH_CAP`].
    ///
    /// NOT on the query path any more: ranking reads the cached, clamped depth
    /// out of [`Self::rank_key`] instead, because this walk costs an `frn_map`
    /// probe per level (~250-400 ns) and the ranking loop used to pay it once
    /// per candidate. This stays as the exact, uncached definition — it is what
    /// the sweep is checked against, and what `path_of`-shaped callers want.
    // Deliberately kept with no non-test caller: this is the SPECIFICATION of
    // the value `rank_key` caches, and the sweep is only trustworthy because a
    // test asserts the two agree for every entry. Step 11 gives it callers
    // again when `search` starts returning slots to the service.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn depth_of(&self, entry_idx: u32) -> u32 {
        let mut depth = 0u32;
        let mut idx = entry_idx;
        while depth < PATH_DEPTH_CAP {
            let e = &self.entries[idx as usize];
            if e.parent_frn == e.frn {
                break;
            }
            match self.frn_map.get(&e.parent_frn) {
                Some(&p) if p != idx => {
                    idx = p;
                    depth += 1;
                }
                _ => break,
            }
        }
        depth
    }

    /// Approximate resident bytes per structure: vec/arena capacities, the
    /// FRN map, and the lazily built matcher structures.
    ///
    /// `frn_map` is charged per hashbrown BUCKET, not per reported capacity.
    /// The table allocates a 16 B `(u64, u32)` payload (8 B key + 4 B value +
    /// 4 B tail padding to the key's alignment) **plus one control byte** for
    /// every bucket, and `capacity()` reports the 7/8 load-factor limit rather
    /// than the bucket count. So `buckets = next_power_of_two(capacity·8/7)`
    /// and the map costs `buckets · 17`. At 1M entries: capacity 1,835,008 →
    /// 2^21 buckets → 35.65 MB, against the 29.36 MB the former
    /// `capacity() · 16` reported — a 21% understatement, and every §3.4
    /// memory-budget line is measured through this function.
    pub fn ram_breakdown(&self) -> RamBreakdown {
        // Deriving buckets from `capacity()` is only sound right after a grow
        // or a shrink. hashbrown's `erase` marks a bucket DELETED rather than
        // EMPTY whenever it sits in a contiguous run of full ones — the common
        // case at a 7/8 load factor — which decrements `items` without
        // crediting `growth_left`. So `capacity()` decays 1:1 with deletions
        // while the allocation does not move, and re-deriving would report a
        // 2^19 table for a 2^21 one: a 4x UNDER-report, the dangerous
        // direction for a hard cap. `frn_map_buckets` is therefore tracked
        // monotonically and only reset where the map actually shrinks.
        let frn_map = (self.frn_map_buckets * (std::mem::size_of::<(u64, u32)>() + 1)) as u64;
        let accel = self.accel_lock().ram_parts();
        RamBreakdown {
            entries: (self.entries.capacity() * std::mem::size_of::<Entry>()) as u64,
            name_arena: self.name_arena.capacity() as u64,
            folded_arena: self.folded_arena.capacity() as u64,
            frn_map,
            free_slots: (self.free_slots.capacity() * std::mem::size_of::<u32>()) as u64,
            rank_key: self.rank_key.capacity() as u64,
            folded_order: accel.folded_order,
            initials: accel.initials,
            trigrams: accel.trigrams,
        }
    }

    /// Total of [`Self::ram_breakdown`]. Feeds `IndexStatus.ram_bytes.filename`.
    pub fn ram_bytes(&self) -> u64 {
        self.ram_breakdown().total()
    }

    /// Live+stale bytes of name payload (`len`, not capacity). Divided by
    /// [`Self::len`] this is the mean name length `L` that rows 2, 3 and 6 of
    /// the §3.4 byte accounting scale through, so it is worth reporting from a
    /// real volume rather than deriving.
    pub fn name_arena_len(&self) -> usize {
        self.name_arena.len()
    }

    /// Live+stale bytes of folded payload, including the one-byte-per-record
    /// [`FOLDED_DELIM`] fences (row 3 of the §3.4 accounting is `L + 1` for
    /// exactly this reason); see [`Self::name_arena_len`].
    pub fn folded_arena_len(&self) -> usize {
        self.folded_arena.len()
    }

    /// Number of folded-arena hits for `needle`, with none of the mapping,
    /// tiering or ranking [`Self::search`] does — the isolated Pass 1 scan.
    /// Instrumentation for the bench harness (§10 M0), not a query API.
    pub fn arena_scan_probe(&self, needle: &str) -> usize {
        crate::matching::arena_scan_probe(self, needle)
    }

    /// Tiered search (§3.4). Returns up to `max_results` hits, best first;
    /// `is_cancelled` is polled at least every 4096 entries/candidates and a
    /// cancelled search returns whatever it has collected so far.
    pub fn search(
        &self,
        query: &str,
        max_results: usize,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Vec<Hit> {
        crate::matching::search(self, query, max_results, is_cancelled)
    }
}

impl crate::EntrySink for VolumeIndex {
    fn add(&mut self, frn: u64, parent_frn: u64, name: &str, flags: u16) {
        self.add_entry(frn, parent_frn, name, flags);
    }

    fn finish(&mut self) {
        self.finalize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{flags, EntrySink};

    fn ix() -> VolumeIndex {
        VolumeIndex::new(0, "C:\\".to_string())
    }

    #[test]
    fn fold_nfc_and_case() {
        assert_eq!(fold("HELLO.TXT"), "hello.txt");
        // NFD "Cafe" + combining acute == NFC "Café" after fold.
        assert_eq!(fold("Cafe\u{301}"), fold("Caf\u{e9}"));
        assert_eq!(fold("Caf\u{c9}"), "caf\u{e9}");
    }

    #[test]
    fn add_len_and_names_are_nfc() {
        let mut v = ix();
        v.add(1, 999, "Cafe\u{301}.txt", 0); // NFD in
        assert_eq!(v.len(), 1);
        assert_eq!(v.name_of(1), Some("Caf\u{e9}.txt")); // NFC out
        assert_eq!(v.name_of(2), None);
    }

    #[test]
    fn path_of_chain() {
        let mut v = ix();
        v.add(10, 5, "foo", flags::DIR);
        v.add(11, 10, "bar", flags::DIR);
        v.add(12, 11, "baz.txt", 0);
        assert_eq!(v.path_of(12).as_deref(), Some("C:\\foo\\bar\\baz.txt"));
        assert_eq!(v.path_of(10).as_deref(), Some("C:\\foo"));
        assert_eq!(v.path_of(999), None);
    }

    #[test]
    fn path_of_missing_parent_anchors_at_root() {
        let mut v = ix();
        v.add(3, 424242, "orphan.txt", 0);
        assert_eq!(v.path_of(3).as_deref(), Some("C:\\orphan.txt"));
    }

    #[test]
    fn path_of_root_without_trailing_backslash() {
        let mut v = VolumeIndex::new(0, "D:".to_string());
        v.add(3, 999, "a.txt", 0);
        assert_eq!(v.path_of(3).as_deref(), Some("D:\\a.txt"));
    }

    #[test]
    fn path_of_self_parent_terminates() {
        let mut v = ix();
        v.add(5, 5, "rootdir", flags::DIR);
        assert_eq!(v.path_of(5).as_deref(), Some("C:\\rootdir"));
        assert_eq!(v.depth_of(v.frn_map[&5]), 0);
    }

    #[test]
    fn path_of_cycle_is_finite() {
        let mut v = ix();
        v.add(1, 2, "a", flags::DIR);
        v.add(2, 1, "b", flags::DIR);
        let p = v.path_of(1).expect("must terminate");
        assert!(p.starts_with("C:\\"));
        // Bounded by the depth cap, not hanging.
        assert!(p.len() <= 3 + 2 * (PATH_DEPTH_CAP as usize + 1));
        assert_eq!(v.depth_of(v.frn_map[&1]), PATH_DEPTH_CAP);
    }

    #[test]
    fn depth_of_counts_parents_walked() {
        let mut v = ix();
        v.add(10, 999, "a", flags::DIR);
        v.add(11, 10, "b", flags::DIR);
        v.add(12, 11, "c.txt", 0);
        assert_eq!(v.depth_of(v.frn_map[&10]), 0);
        assert_eq!(v.depth_of(v.frn_map[&12]), 2);
    }

    #[test]
    fn apply_create_delete_fixes_frn_map() {
        let mut v = ix();
        v.apply(UsnEvent::Create {
            frn: 1,
            parent_frn: 999,
            name: "one.txt".into(),
            flags: 0,
        });
        v.apply(UsnEvent::Create {
            frn: 2,
            parent_frn: 999,
            name: "two.txt".into(),
            flags: 0,
        });
        v.apply(UsnEvent::Create {
            frn: 3,
            parent_frn: 999,
            name: "three.txt".into(),
            flags: 0,
        });
        v.apply(UsnEvent::Delete { frn: 1 });
        assert_eq!(v.len(), 2);
        assert_eq!(v.path_of(1), None);
        // frn 1's slot is tombstoned in place — nothing else moves, so every
        // survivor keeps its slot and still resolves.
        assert_eq!(v.path_of(3).as_deref(), Some("C:\\three.txt"));
        assert_eq!(v.path_of(2).as_deref(), Some("C:\\two.txt"));
        v.apply(UsnEvent::Delete { frn: 42 }); // unknown: no-op
        assert_eq!(v.len(), 2);
    }

    /// The regression test for the whole slot-stability model: a delete
    /// tombstones its slot, the next create takes that exact slot back, and
    /// nothing — the FRN map, the arenas, any matcher structure keyed by slot
    /// — may attribute a result to the entry that used to live there.
    #[test]
    fn recycled_slot_does_not_misattribute_results() {
        let mut v = ix();
        for (frn, name) in [
            (1u64, "alpha.txt"),
            (2, "beta.txt"),
            (3, "gamma.txt"),
            (4, "delta.txt"),
        ] {
            v.add(frn, 999, name, 0);
        }
        let freed = v.frn_map[&2];
        v.apply(UsnEvent::Delete { frn: 2 });
        assert_eq!(v.len(), 3);
        assert_eq!(v.entries.len(), 4, "the tombstone keeps its slot");
        assert_eq!(v.dead_bytes(), 2 * "beta.txt".len(), "both arenas charged");

        v.apply(UsnEvent::Create {
            frn: 5,
            parent_frn: 999,
            name: "epsilon.txt".into(),
            flags: 0,
        });
        assert_eq!(v.frn_map[&5], freed, "the freed slot must be recycled");
        assert!(v.free_slots.is_empty());
        assert_eq!(v.len(), 4);
        assert_eq!(v.entries.len(), 4, "recycling must not grow the table");

        // The recycled entry and every survivor answer as themselves.
        for (frn, name, stem) in [
            (1u64, "alpha.txt", "alpha"),
            (3, "gamma.txt", "gamma"),
            (4, "delta.txt", "delta"),
            (5, "epsilon.txt", "epsilon"),
        ] {
            let hits = v.search(stem, 10, &|| false);
            assert_eq!(hits.len(), 1, "{stem}");
            assert_eq!(hits[0].frn, frn, "{stem}");
            assert_eq!(v.name_of(frn), Some(name));
            let want = format!("C:\\{name}");
            assert_eq!(v.path_of(frn).as_deref(), Some(want.as_str()));
        }
        // The deleted name is gone from every tier despite its arena bytes
        // still sitting in front of the recycled entry's.
        assert!(v.search("beta", 10, &|| false).is_empty());
        assert_eq!(v.name_of(2), None);
        assert_eq!(v.path_of(2), None);
    }

    /// Parent FRN no entry in these tests ever mints, so an entry naming it
    /// anchors at the root instead of nesting under a sibling.
    const ABSENT_PARENT: u64 = 9_999_999;

    fn rank_depth(v: &VolumeIndex, frn: u64) -> u8 {
        v.rank_key[v.frn_map[&frn] as usize] & RANK_DEPTH_MAX
    }

    /// The cached ranking depth must equal the exact parent walk — clamped to
    /// the 7 bits it lives in — for EVERY entry, whatever order the entries
    /// arrived in.
    ///
    /// Insertion order is the load-bearing part. `FSCTL_ENUM_USN_DATA` walks in
    /// MFT-record order, so a child enumerated before its parent is the normal
    /// case, not a corner one; the incremental rule in `register_slot` caches 0
    /// for all of them, and only `finalize`'s sweep can fix it. Without the
    /// sweep a file twenty levels down would rank as if it sat at the volume
    /// root, and nothing else in the suite would notice.
    #[test]
    fn cached_depth_matches_depth_of_after_finalize() {
        let mut v = ix();

        // (1) A chain inserted CHILD FIRST: every add sees a parent that does
        // not exist yet.
        const CHAIN: u64 = 12;
        for d in (0..=CHAIN).rev() {
            let parent = if d == 0 { ABSENT_PARENT } else { 100 + d - 1 };
            v.add(100 + d, parent, &format!("dir{d}"), flags::DIR);
        }
        assert_eq!(rank_depth(&v, 100 + CHAIN), 0, "child-first caches 0…");
        assert_eq!(
            v.depth_of(v.frn_map[&(100 + CHAIN)]),
            CHAIN as u32,
            "…wrongly"
        );

        // (2) Parent-first, with the hidden/system bit in play.
        v.add(200, ABSENT_PARENT, "top", flags::DIR);
        v.add(201, 200, "mid", flags::DIR | flags::HIDDEN);
        v.add(202, 201, "leaf.txt", flags::SYSTEM);
        // (3) A self-parenting volume root, something under it, and an orphan.
        v.add(300, 300, "root", flags::DIR);
        v.add(301, 300, "under-root.txt", 0);
        v.add(400, 424_242, "orphan.txt", 0);
        // (4) A chain deeper than the 7-bit clamp.
        for d in 0..200u64 {
            let parent = if d == 0 { ABSENT_PARENT } else { 1000 + d - 1 };
            v.add(1000 + d, parent, &format!("d{d}"), flags::DIR);
        }
        // (5) A parent cycle, which `depth_of` reports as PATH_DEPTH_CAP.
        v.add(500, 501, "cyc-a", flags::DIR);
        v.add(501, 500, "cyc-b", flags::DIR);

        v.finalize();

        for (slot, e) in v.live_entries() {
            assert_eq!(
                (v.rank_key[slot as usize] & RANK_DEPTH_MAX) as u32,
                v.depth_of(slot).min(RANK_DEPTH_MAX as u32),
                "cached depth for frn {} disagrees with the walk",
                e.frn
            );
            assert_eq!(
                v.rank_key[slot as usize] & RANK_PENALIZED != 0,
                e.flags & (flags::HIDDEN | flags::SYSTEM) != 0,
                "hidden/system bit for frn {}",
                e.frn
            );
        }

        assert_eq!(rank_depth(&v, 100 + CHAIN), CHAIN as u8);
        assert_eq!(rank_depth(&v, 202), 2);
        assert_eq!(v.rank_key[v.frn_map[&202] as usize] & RANK_PENALIZED, 0x80);
        assert_eq!(rank_depth(&v, 301), 1);
        assert_eq!(rank_depth(&v, 400), 0);
        // Clamped, not wrapped: 199 and 512 both land on 127.
        assert_eq!(rank_depth(&v, 1199), RANK_DEPTH_MAX);
        assert_eq!(rank_depth(&v, 500), RANK_DEPTH_MAX);
        assert_eq!(rank_depth(&v, 501), RANK_DEPTH_MAX);
    }

    /// A directory reparent shifts every descendant's cached depth by a
    /// constant. It is repaired on the WRITER, debounced and sliced — never
    /// from `search`, because a depth refresh on the query path re-arms the
    /// rebuild-on-mutation cliff the column exists to remove.
    #[test]
    fn directory_reparent_is_repaired_by_the_writer_not_by_search() {
        let mut v = ix();
        v.add(10, ABSENT_PARENT, "top", flags::DIR);
        v.add(11, ABSENT_PARENT, "other", flags::DIR);
        v.add(20, 10, "sub", flags::DIR);
        v.add(30, 20, "deeper", flags::DIR);
        v.add(31, 30, "x.txt", 0);
        v.add(21, 20, "leaf.txt", 0);
        v.finalize();
        assert_eq!(
            (rank_depth(&v, 20), rank_depth(&v, 30), rank_depth(&v, 31)),
            (1, 2, 3)
        );

        // Moving a FILE arms nothing: it has no descendants, and its own depth
        // is recomputed where it is re-registered.
        v.apply(UsnEvent::Rename {
            frn: 21,
            new_parent_frn: 11,
            new_name: "leaf.txt".into(),
        });
        assert!(
            v.depth_repair_armed.is_none(),
            "a file move has no descendants"
        );
        assert_eq!(rank_depth(&v, 21), 1);
        assert!(!v.repair_depths_slice(), "nothing armed, nothing to do");

        // Moving a DIRECTORY does. The moved entry itself is fixed at once;
        // its descendants are what go stale.
        v.apply(UsnEvent::Rename {
            frn: 20,
            new_parent_frn: ABSENT_PARENT,
            new_name: "sub".into(),
        });
        assert!(v.depth_repair_armed.is_some());
        assert!(!v.depth_repair_due(), "the debounce has not elapsed");
        assert_eq!(
            rank_depth(&v, 20),
            0,
            "the moved entry is exact immediately"
        );
        assert_eq!(
            (rank_depth(&v, 30), rank_depth(&v, 31)),
            (2, 3),
            "descendants are stale"
        );
        // Search does NOT repair: it reads rank_key unconditionally.
        assert_eq!(v.search("deeper", 10, &|| false).len(), 1);
        assert_eq!(rank_depth(&v, 30), 2, "the query path must not sweep");

        // The writer drains it in slices; one is enough at this size.
        while v.repair_depths_slice() {}
        assert!(v.depth_repair_armed.is_none());
        assert_eq!((rank_depth(&v, 30), rank_depth(&v, 31)), (1, 2));
        for (slot, _) in v.live_entries() {
            assert_eq!(
                (v.rank_key[slot as usize] & RANK_DEPTH_MAX) as u32,
                v.depth_of(slot).min(RANK_DEPTH_MAX as u32)
            );
        }
    }

    #[test]
    fn apply_create_existing_frn_updates() {
        let mut v = ix();
        v.add(1, 999, "a.txt", 0);
        let slot = v.frn_map[&1];
        v.apply(UsnEvent::Create {
            frn: 1,
            parent_frn: 999,
            name: "b.txt".into(),
            flags: flags::HIDDEN,
        });
        assert_eq!(v.len(), 1);
        assert_eq!(v.name_of(1), Some("b.txt"));
        // A create for a known FRN renames in place: same slot, no tombstone,
        // and the replaced name's bytes charged as dead.
        assert_eq!(v.frn_map[&1], slot);
        assert_eq!(v.entries.len(), 1);
        assert!(v.free_slots.is_empty());
        assert_eq!(v.dead_bytes(), 2 * "a.txt".len());
        assert!(v.search("a.txt", 10, &|| false).is_empty());
    }

    #[test]
    fn apply_rename_updates_name_and_parent() {
        let mut v = ix();
        v.add(10, 999, "dir", flags::DIR);
        v.add(7, 999, "old.txt", 0);
        v.apply(UsnEvent::Rename {
            frn: 7,
            new_parent_frn: 10,
            new_name: "new.txt".into(),
        });
        assert_eq!(v.name_of(7), Some("new.txt"));
        assert_eq!(v.path_of(7).as_deref(), Some("C:\\dir\\new.txt"));
        // Search sees the rename (accel rebuilds lazily).
        let hits = v.search("new", 10, &|| false);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].frn, 7);
        assert!(v.search("old", 10, &|| false).is_empty());
        // Rename of an unknown FRN is ignored.
        v.apply(UsnEvent::Rename {
            frn: 555,
            new_parent_frn: 999,
            new_name: "x".into(),
        });
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn security_change_is_noop() {
        let mut v = ix();
        v.add(1, 999, "a.txt", 0);
        v.apply(UsnEvent::SecurityChange { frn: 1 });
        assert_eq!(v.len(), 1);
        assert_eq!(v.name_of(1), Some("a.txt"));
    }

    #[test]
    fn empty_name_is_skipped() {
        let mut v = ix();
        v.add(1, 999, "", 0);
        assert_eq!(v.len(), 0);
    }

    /// Invariant I3: nothing below 0x20 reaches the folded arena except the
    /// record delimiters, so a name whose folded form carries a control byte
    /// is dropped at `intern` alongside the empty-name drop. NTFS forbids
    /// these bytes, so only synthetic and walk-mode inputs can hit this.
    #[test]
    fn control_byte_name_is_skipped() {
        let mut v = ix();
        v.add(1, 999, "nul\u{0}name.txt", 0); // the delimiter itself
        v.add(2, 999, "bell\u{7}.txt", 0);
        v.add(3, 999, "tab\there.txt", 0);
        v.add(4, 999, "\u{1f}.txt", 0);
        assert_eq!(v.len(), 0);
        assert_eq!(v.name_of(1), None);
        // Dropped BEFORE either arena is touched: a rejected name must not
        // leave bytes behind for the next record's offsets to sit after.
        assert!(v.name_arena.is_empty());
        assert!(v.folded_arena.is_empty());
        assert_eq!(v.dead_bytes(), 0);
        // DEL (0x7f) and above are not control bytes by this rule, and a
        // clean name is still accepted.
        v.add(5, 999, "ok\u{7f}.txt", 0);
        v.add(6, 999, "plain.txt", 0);
        assert_eq!(v.len(), 2);
        assert_eq!(v.name_of(6), Some("plain.txt"));
    }

    /// The folded arena is `0x00 rec 0x00 rec 0x00` (design §2): every record
    /// is fenced, `folded_off` points past its leading NUL, and the trailing
    /// NUL is what makes `arena[hit + qlen]` in bounds for tier
    /// classification. Delete and rename NUL-fill the vacated record so dead
    /// bytes are inert rather than merely bounds-checked (invariant I4).
    #[test]
    fn folded_arena_is_nul_fenced_and_dead_records_are_erased() {
        let mut v = ix();
        v.add(1, 999, "Alpha", 0);
        v.add(2, 999, "Beta", 0);
        v.add(3, 999, "Gamma", 0);
        assert_eq!(v.folded_arena, "\0alpha\0beta\0gamma\0");

        let check_fenced = |v: &VolumeIndex| {
            let a = v.folded_arena.as_bytes();
            assert_eq!(a.first(), Some(&0), "leading delimiter");
            assert_eq!(a.last(), Some(&0), "trailing delimiter");
            for (_, e) in v.live_entries() {
                let off = e.folded_off as usize;
                let end = off + e.folded_len as usize;
                assert_eq!(a[off - 1], 0, "record is preceded by a delimiter");
                assert_eq!(a[end], 0, "record is followed by a delimiter");
                assert!(a[off..end].iter().all(|&b| b >= 0x20), "no control bytes");
            }
        };
        check_fenced(&v);

        // Delete erases "beta" in place. Nothing shifts, so every surviving
        // record keeps its offset; the erased record just merges with the two
        // fences around it into one longer run of delimiters.
        v.apply(UsnEvent::Delete { frn: 2 });
        assert_eq!(v.folded_arena, format!("\0alpha{}gamma\0", "\0".repeat(6)));
        check_fenced(&v);

        // Rename appends the new record and erases the old one.
        v.apply(UsnEvent::Rename {
            frn: 3,
            new_parent_frn: 999,
            new_name: "Delta".into(),
        });
        assert_eq!(v.folded_arena, format!("\0alpha{}delta\0", "\0".repeat(12)));
        check_fenced(&v);
        assert_eq!(v.name_of(3), Some("Delta"));
        assert!(v.search("gamma", 10, &|| false).is_empty());
        assert_eq!(v.search("delta", 10, &|| false).len(), 1);
    }

    #[test]
    fn ram_bytes_grows() {
        let mut v = ix();
        let before = v.ram_bytes();
        for i in 0..100u64 {
            v.add(i + 1, 999, &format!("file-{i}.txt"), 0);
        }
        assert!(v.ram_bytes() > before);
    }

    #[test]
    fn ram_breakdown_sums_and_charges_hash_control_bytes() {
        let mut v = ix();
        assert_eq!(v.ram_breakdown(), RamBreakdown::default()); // nothing allocated yet
        for i in 0..1000u64 {
            v.add(i + 1, 999, &format!("file-{i}.txt"), 0);
        }
        let b = v.ram_breakdown();
        assert_eq!(b.total(), v.ram_bytes());
        assert_eq!(
            b.entries,
            (v.entries.capacity() * std::mem::size_of::<Entry>()) as u64
        );
        assert_eq!(b.name_arena, v.name_arena.capacity() as u64);
        // Charged per bucket (17 B), not per reported capacity (16 B), and
        // capacity() is only 7/8 of the buckets — so strictly more than the
        // pre-fix `capacity() * 16`.
        assert!(b.frn_map > (v.frn_map.capacity() * 16) as u64);
        // Accel is untouched until the first search.
        assert_eq!((b.folded_order, b.initials, b.trigrams), (0, 0, 0));
        assert!(!v.search("file-7", 4, &|| false).is_empty());
        assert!(v.ram_breakdown().initials > 0);
    }

    #[test]
    fn finalize_releases_slack_and_growth_is_chunked() {
        let mut v = ix();
        // Parent FRN deliberately outside the range this loop mints, so every
        // entry anchors at the root: FRN 999 would otherwise be `file-998.txt`
        // and the paths below would nest under it.
        const ABSENT_PARENT: u64 = 90_000;
        for i in 0..1000u64 {
            v.add(i + 1, ABSENT_PARENT, &format!("file-{i}.txt"), 0);
        }
        let grown = v.ram_bytes();
        v.finalize();
        let settled = v.ram_bytes();
        assert!(settled < grown, "finalize must hand back doubling slack");

        // One chunk covers many inserts: the second add allocates nothing.
        v.add(5001, ABSENT_PARENT, "after-finalize-one.txt", 0);
        let after_first = v.ram_bytes();
        v.add(5002, ABSENT_PARENT, "after-finalize-two.txt", 0);
        assert_eq!(v.ram_bytes(), after_first);
        assert_eq!(v.len(), 1002);
        assert_eq!(v.name_of(5002), Some("after-finalize-two.txt"));
        assert_eq!(
            v.path_of(5001).as_deref(),
            Some("C:\\after-finalize-one.txt")
        );
        assert_eq!(v.search("after-finalize-two", 4, &|| false).len(), 1);
    }

    #[test]
    fn finish_finalizes_the_index() {
        let mut v = ix();
        for i in 0..1000u64 {
            v.add(i + 1, 999, &format!("file-{i}.txt"), 0);
        }
        let grown = v.ram_bytes();
        v.finish(); // EntrySink's default is a no-op; VolumeIndex overrides it
        assert!(v.ram_bytes() < grown);
    }

    #[test]
    fn arena_scan_probe_counts_folded_hits_only() {
        let mut v = ix();
        v.add(1, 999, "Alpha.txt", 0);
        v.add(2, 999, "alphabet.txt", 0);
        assert_eq!(v.arena_scan_probe("ALPHA"), 2); // folded, so case-blind
        assert_eq!(v.arena_scan_probe("zqxjvw-no-such-name"), 0);
        assert_eq!(v.arena_scan_probe(""), 0);
    }
}
