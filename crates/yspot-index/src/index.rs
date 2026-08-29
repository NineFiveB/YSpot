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

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use unicode_normalization::UnicodeNormalization;

use crate::matching::Accel;
use crate::{Hit, UsnEvent};

/// Cycle/depth guard for parent-chain walks. NTFS practical depth is far
/// below this; the cap only defends against corrupt/cyclic parent chains.
pub(crate) const PATH_DEPTH_CAP: u32 = 512;

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
    /// Lazily rebuilt matcher acceleration structures (§3.4 prefilters).
    /// Mutex (not RefCell) so `search(&self)` stays `Sync`-safe behind an
    /// outer `RwLock` in the service.
    pub(crate) accel: Mutex<Accel>,
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
            accel: Mutex::new(Accel::new()),
        }
    }

    pub fn volume_idx(&self) -> u32 {
        self.volume_idx
    }

    pub fn root_path(&self) -> &str {
        &self.root_path
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
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
        let name_off = self.name_arena.len() as u32;
        self.name_arena.push_str(nfc);
        let folded_off = self.folded_arena.len() as u32;
        self.folded_arena.push_str(folded);
        Some((name_off, nfc.len() as u16, folded_off, folded.len() as u16))
    }

    /// Add one entry; an existing FRN is updated in place (new name appended
    /// to the arenas — old bytes leak until rebuild, §3.7).
    pub(crate) fn add_entry(&mut self, frn: u64, parent_frn: u64, name: &str, flags: u16) {
        if !self.frn_map.contains_key(&frn) && self.entries.len() >= u32::MAX as usize {
            log::error!("index: entry table full; frn {frn:#x} dropped");
            return;
        }
        let Some((name_off, name_len, folded_off, folded_len)) = self.intern(name) else {
            log::warn!("index: skipping unusable name for frn {frn:#x}");
            return;
        };
        if let Some(&idx) = self.frn_map.get(&frn) {
            let e = &mut self.entries[idx as usize];
            e.parent_frn = parent_frn;
            e.flags = flags;
            e.name_off = name_off;
            e.name_len = name_len;
            e.folded_off = folded_off;
            e.folded_len = folded_len;
        } else {
            let idx = self.entries.len() as u32;
            self.entries.push(Entry {
                frn,
                parent_frn,
                name_off,
                folded_off,
                name_len,
                folded_len,
                flags,
            });
            self.frn_map.insert(frn, idx);
        }
        self.mark_dirty();
    }

    fn remove_entry(&mut self, frn: u64) {
        let Some(idx) = self.frn_map.remove(&frn) else {
            return;
        };
        // Swap-remove keeps the table dense; re-point the moved entry's FRN.
        // The removed name's arena bytes leak until rebuild (§3.7).
        self.entries.swap_remove(idx as usize);
        if (idx as usize) < self.entries.len() {
            let moved_frn = self.entries[idx as usize].frn;
            self.frn_map.insert(moved_frn, idx);
        }
        self.mark_dirty();
    }

    fn rename_entry(&mut self, frn: u64, new_parent_frn: u64, new_name: &str) {
        if !self.frn_map.contains_key(&frn) {
            log::debug!("index: rename for unknown frn {frn:#x} ignored");
            return;
        }
        let Some((name_off, name_len, folded_off, folded_len)) = self.intern(new_name) else {
            log::warn!("index: rename to unusable name for frn {frn:#x} ignored");
            return;
        };
        let idx = self.frn_map[&frn];
        let e = &mut self.entries[idx as usize];
        e.parent_frn = new_parent_frn;
        e.name_off = name_off;
        e.name_len = name_len;
        e.folded_off = folded_off;
        e.folded_len = folded_len;
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

    /// Approximate resident bytes: vec capacities, arenas, FRN map, and the
    /// lazily built matcher structures. Feeds `IndexStatus.ram_bytes.filename`.
    pub fn ram_bytes(&self) -> u64 {
        let entries = self.entries.capacity() * std::mem::size_of::<Entry>();
        let arenas = self.name_arena.capacity() + self.folded_arena.capacity();
        // hashbrown: 12 B payload + control byte per slot, rounded up.
        let frn_map = self.frn_map.capacity() * 16;
        (entries + arenas + frn_map) as u64 + self.accel_lock().ram_bytes()
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
        // swap_remove moved frn 3 into slot 0; the map must still resolve it.
        assert_eq!(v.path_of(3).as_deref(), Some("C:\\three.txt"));
        assert_eq!(v.path_of(2).as_deref(), Some("C:\\two.txt"));
        v.apply(UsnEvent::Delete { frn: 42 }); // unknown: no-op
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn apply_create_existing_frn_updates() {
        let mut v = ix();
        v.add(1, 999, "a.txt", 0);
        v.apply(UsnEvent::Create {
            frn: 1,
            parent_frn: 999,
            name: "b.txt".into(),
            flags: flags::HIDDEN,
        });
        assert_eq!(v.len(), 1);
        assert_eq!(v.name_of(1), Some("b.txt"));
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
}
