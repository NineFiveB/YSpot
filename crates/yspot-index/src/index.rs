//! In-memory per-volume filename index (SPEC §3.4).
//!
//! Layout: one fixed-size [`Entry`] per file/dir; original-case names (UTF-8,
//! NFC-normalized) in one contiguous arena plus a case-folded shadow arena for
//! matching; an FRN → entry-index map for USN application and path
//! reconstruction. Full paths are never stored — [`VolumeIndex::path_of`]
//! walks the `parent_frn` chain to the volume root on demand.
//!
//! Mutations (`add`/`apply`) only append to the arenas; bytes belonging to
//! deleted or renamed entries leak until the next full rebuild/snapshot cycle
//! (§3.7). This keeps USN application O(1) and is bounded by rebuild cadence.
//!
//! Slot stability: `entries[i]` belongs to one file for its lifetime. A delete
//! tombstones the slot ([`crate::flags::DEAD`]) and offers it to `free_slots`;
//! it never moves another entry. That is what allows structures to be keyed by
//! entry index, and it is why `len()` is a maintained live count rather than
//! `entries.len()`.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use unicode_normalization::UnicodeNormalization;

use crate::matching::Accel;
use crate::{Hit, UsnEvent};

/// Cycle/depth guard for parent-chain walks. NTFS practical depth is far
/// below this; the cap only defends against corrupt/cyclic parent chains.
pub(crate) const PATH_DEPTH_CAP: u32 = 512;

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
    /// Case-folded shadow of `name_arena` (offsets differ; folding expands).
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
    pub fn finalize(&mut self) {
        self.entries.shrink_to_fit();
        self.name_arena.shrink_to_fit();
        self.folded_arena.shrink_to_fit();
        self.frn_map.shrink_to_fit();
        // The one place the map's allocation actually gets smaller, so the one
        // place the monotonic bucket count may be lowered.
        self.frn_map_buckets = 0;
        self.note_frn_map_buckets();
        self.settled = true;
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
    /// unusable names (empty after normalization, or arena offsets exhausted).
    fn intern(&mut self, name: &str) -> Option<(u32, u16, u32, u16)> {
        let nfc: String = name.nfc().collect();
        let nfc = truncate_to_boundary(&nfc, u16::MAX as usize);
        if nfc.is_empty() {
            return None;
        }
        let folded_full = fold(nfc);
        let folded = truncate_to_boundary(&folded_full, u16::MAX as usize);
        if self.name_arena.len() + nfc.len() > u32::MAX as usize
            || self.folded_arena.len() + folded.len() > u32::MAX as usize
        {
            log::error!("index: name arena offset space exhausted; entry dropped");
            return None;
        }
        if self.settled {
            Self::reserve_arena(&mut self.name_arena, nfc.len());
            Self::reserve_arena(&mut self.folded_arena, folded.len());
        }
        let name_off = self.name_arena.len() as u32;
        self.name_arena.push_str(nfc);
        let folded_off = self.folded_arena.len() as u32;
        self.folded_arena.push_str(folded);
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
            if self.settled && self.entries.len() == self.entries.capacity() {
                self.entries.reserve_exact(ENTRY_GROW_CHUNK);
            }
            self.entries.push(e);
        } else {
            self.entries[slot as usize] = e;
        }
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

    /// Tombstone `slot`: drop it from `frn_map`, mark it [`crate::flags::DEAD`]
    /// and charge its arena bytes to `dead_bytes`. The entry stays where it
    /// is — nothing is ever swap-removed, because that would permute every
    /// other entry's index.
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
        self.frn_map.remove(&frn);
        self.dead_bytes += bytes;
        self.live_count -= 1;
        debug_assert_eq!(self.live_count, self.frn_map.len());
    }

    /// Add one entry; an existing FRN is updated in place (new name appended
    /// to the arenas — old bytes leak until rebuild, §3.7).
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
                self.unregister_slot(idx);
                idx
            }
            None => self.free_slots.pop().unwrap_or(self.entries.len() as u32),
        };
        self.register_slot(slot, frn, parent_frn, interned, flags);
        self.mark_dirty();
    }

    fn remove_entry(&mut self, frn: u64) {
        let Some(&idx) = self.frn_map.get(&frn) else {
            return;
        };
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

    /// Number of parent links successfully followed from `entry_idx`
    /// (the §3.4 `depth_penalty` input), capped at [`PATH_DEPTH_CAP`].
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

    /// Live+stale bytes of folded payload; see [`Self::name_arena_len`].
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
