//! Tiered matcher and candidate prefilters (SPEC §3.4).
//!
//! Tier order (base match quality):
//! exact `1.0` > prefix `0.9` > word-boundary segment start `0.8` >
//! camel/initials `0.7` > contiguous substring `0.55` > fuzzy subsequence
//! `0.3..=0.5` (by density `matched_len / span`).
//!
//! Final score = `base × depth_penalty × hidden_penalty` where
//! `depth_penalty = 1 / (1 + 0.02·path_depth)` and entries carrying the
//! HIDDEN or SYSTEM attribute are additionally multiplied by `0.85`.
//!
//! Candidate generation never walks the whole entry table per tier:
//! exact/prefix/word-boundary/substring fall out of one `memmem` scan over
//! the folded name arena (hits mapped to entries via a sorted offset vec);
//! initials are substring-scanned in their own small arena; fuzzy candidates
//! come from byte-trigram posting-list intersection, capped at
//! [`FUZZY_CAP`] scored candidates per query.
//!
//! `match_ranges` are UTF-16 code-unit ranges into the ORIGINAL (NFC) name
//! (§5.13), computed only for the final top-K page via [`fold_with_map`].

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap};

use memchr::memmem;

use crate::index::{fold, VolumeIndex};
use crate::{flags, Hit};

/// Cancellation is polled at least every this many hits/candidates (§3.4
/// mandates prompt cancel on new keystrokes) and before every pass.
const CANCEL_STRIDE: usize = 4096;

/// Max fuzzy candidates scored per query (§3.4 prefilter bound).
const FUZZY_CAP: usize = 20_000;

fn is_sep_byte(b: u8) -> bool {
    matches!(b, b'-' | b'_' | b'.' | b' ')
}

fn is_sep_char(c: char) -> bool {
    matches!(c, '-' | '_' | '.' | ' ')
}

/// Matcher acceleration structures, owned by the index behind a `Mutex` and
/// rebuilt lazily on the first search after any mutation (`dirty`).
pub(crate) struct Accel {
    /// Set by index mutations; cleared by [`Accel::ensure_basic`].
    pub(crate) dirty: bool,
    /// `(folded_off, entry_idx)` for every live entry, sorted by offset —
    /// maps folded-arena hit offsets back to entries via binary search.
    folded_order: Vec<(u32, u32)>,
    /// First char of each name segment (folded), all entries back to back.
    initials_arena: String,
    /// Per-entry span of `initials_arena`, in ascending offset order.
    initials_spans: Vec<InitialsSpan>,
    /// Byte-trigram posting lists over folded names; entry indices ascending.
    /// Built lazily on the first fuzzy query after changes.
    trigrams: Option<HashMap<[u8; 3], Vec<u32>>>,
}

#[derive(Debug, Clone, Copy)]
struct InitialsSpan {
    off: u32,
    len: u32,
    entry: u32,
}

impl Accel {
    pub(crate) fn new() -> Self {
        Self {
            dirty: true,
            folded_order: Vec::new(),
            initials_arena: String::new(),
            initials_spans: Vec::new(),
            trigrams: None,
        }
    }

    fn ensure_basic(&mut self, ix: &VolumeIndex) {
        if !self.dirty {
            return;
        }
        self.folded_order.clear();
        self.folded_order.reserve(ix.entries.len());
        for (i, e) in ix.entries.iter().enumerate() {
            self.folded_order.push((e.folded_off, i as u32));
        }
        self.folded_order.sort_unstable();

        self.initials_arena.clear();
        self.initials_spans.clear();
        self.initials_spans.reserve(ix.entries.len());
        for (i, e) in ix.entries.iter().enumerate() {
            let name = ix.name_of_entry(e);
            let off = self.initials_arena.len() as u32;
            for (seg_start, _) in segment_spans(name) {
                if let Some(c) = name[seg_start..].chars().next() {
                    for lc in c.to_lowercase() {
                        self.initials_arena.push(lc);
                    }
                }
            }
            let len = self.initials_arena.len() as u32 - off;
            self.initials_spans.push(InitialsSpan {
                off,
                len,
                entry: i as u32,
            });
        }

        self.trigrams = None; // rebuilt lazily on the next fuzzy query
        self.dirty = false;
    }

    fn ensure_trigrams(&mut self, ix: &VolumeIndex) {
        if self.trigrams.is_some() {
            return;
        }
        let mut map: HashMap<[u8; 3], Vec<u32>> = HashMap::new();
        for (i, e) in ix.entries.iter().enumerate() {
            let bytes = ix.folded_of_entry(e).as_bytes();
            for w in bytes.windows(3) {
                let list = map.entry([w[0], w[1], w[2]]).or_default();
                if list.last().copied() != Some(i as u32) {
                    list.push(i as u32);
                }
            }
        }
        self.trigrams = Some(map);
    }

    pub(crate) fn ram_bytes(&self) -> u64 {
        let mut b = self.folded_order.capacity() * std::mem::size_of::<(u32, u32)>()
            + self.initials_arena.capacity()
            + self.initials_spans.capacity() * std::mem::size_of::<InitialsSpan>();
        if let Some(tg) = &self.trigrams {
            // key + bucket + Vec header per posting list, plus list payloads.
            b += tg.len() * (3 + 8 + std::mem::size_of::<Vec<u32>>());
            b += tg.values().map(|v| v.capacity() * 4).sum::<usize>();
        }
        b as u64
    }
}

/// Byte spans of name segments in the ORIGINAL (NFC, original-case) name:
/// maximal runs of non-separator chars (`-`, `_`, `.`, space), additionally
/// split at lower→upper camel transitions (`FooBar` → `Foo`, `Bar`).
pub(crate) fn segment_spans(name: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    let mut prev: Option<char> = None;
    for (i, c) in name.char_indices() {
        if is_sep_char(c) {
            if let Some(s) = start.take() {
                spans.push((s, i));
            }
        } else {
            let camel = matches!(prev, Some(p) if p.is_lowercase() && c.is_uppercase());
            if camel {
                if let Some(s) = start.take() {
                    spans.push((s, i));
                }
                start = Some(i);
            } else if start.is_none() {
                start = Some(i);
            }
        }
        prev = Some(c);
    }
    if let Some(s) = start {
        spans.push((s, name.len()));
    }
    spans
}

/// Fold an already-NFC name char-wise (same fold as [`fold`] given NFC input)
/// and record, for every byte of the folded output, the UTF-16 code-unit
/// offset of the source char in `name`. Used only for the final top-K page
/// to turn folded-byte match spans into §5.13 UTF-16 ranges.
pub(crate) fn fold_with_map(name: &str) -> (String, Vec<u32>) {
    let mut folded = String::new();
    let mut map = Vec::with_capacity(name.len());
    let mut u16_off: u32 = 0;
    for c in name.chars() {
        for lc in c.to_lowercase() {
            let before = folded.len();
            folded.push(lc);
            for _ in before..folded.len() {
                map.push(u16_off);
            }
        }
        u16_off += c.len_utf16() as u32;
    }
    (folded, map)
}

fn utf16_len(name: &str) -> u32 {
    name.encode_utf16().count() as u32
}

/// UTF-16 offset in `name` of the char starting at `byte` (a char boundary).
fn utf16_offset_at_byte(name: &str, byte: usize) -> u32 {
    let mut off = 0u32;
    for (i, c) in name.char_indices() {
        if i >= byte {
            break;
        }
        off += c.len_utf16() as u32;
    }
    off
}

/// UTF-16 end offset (exclusive) of the source char that produced folded
/// byte `last_byte`: the next distinct map value, or `total` at the end.
fn utf16_end(map: &[u32], total: u32, last_byte: usize) -> u32 {
    let s = map[last_byte];
    map[last_byte + 1..]
        .iter()
        .copied()
        .find(|&v| v > s)
        .unwrap_or(total)
}

/// Merge ranges that touch or overlap (sorts first).
fn merge_ranges(mut v: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
    v.sort_unstable();
    let mut out: Vec<(u32, u32)> = Vec::with_capacity(v.len());
    for (s, e) in v {
        match out.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
    Exact,
    Prefix,
    WordBoundary,
    Initials,
    Substring,
    Fuzzy,
}

fn tier_base(t: Tier) -> f32 {
    match t {
        Tier::Exact => 1.0,
        Tier::Prefix => 0.9,
        Tier::WordBoundary => 0.8,
        Tier::Initials => 0.7,
        Tier::Substring => 0.55,
        Tier::Fuzzy => 0.3, // floor; actual fuzzy base is density-scaled
    }
}

#[derive(Debug, Clone, Copy)]
struct Cand {
    tier: Tier,
    base: f32,
}

/// Dedup across tiers by entry, keeping the best base score.
fn upgrade(best: &mut HashMap<u32, Cand>, entry: u32, tier: Tier, base: f32) {
    best.entry(entry)
        .and_modify(|c| {
            if base > c.base {
                *c = Cand { tier, base };
            }
        })
        .or_insert(Cand { tier, base });
}

/// Map a folded-arena hit offset to the entry containing it, if the whole
/// match lies inside that entry's folded name (stale bytes from renamed or
/// deleted names fail the bounds check and are skipped).
fn entry_at(order: &[(u32, u32)], ix: &VolumeIndex, hit: usize, qlen: usize) -> Option<u32> {
    let pos = order.partition_point(|&(off, _)| (off as usize) <= hit);
    if pos == 0 {
        return None;
    }
    let (off, eidx) = order[pos - 1];
    let e = &ix.entries[eidx as usize];
    if hit + qlen <= off as usize + e.folded_len as usize {
        Some(eidx)
    } else {
        None
    }
}

fn initials_entry_at(spans: &[InitialsSpan], hit: usize, qlen: usize) -> Option<u32> {
    let pos = spans.partition_point(|s| (s.off as usize) <= hit);
    if pos == 0 {
        return None;
    }
    let s = spans[pos - 1];
    if hit + qlen <= s.off as usize + s.len as usize {
        Some(s.entry)
    } else {
        None
    }
}

/// Subsequence match density of folded query `q` in folded `name`:
/// `matched_len / span` in chars, using the earliest-end window tightened
/// backward (bounded two-pass, O(|name|)). `None` if `q` is not a
/// subsequence of `name`.
fn fuzzy_density(name: &str, q: &str) -> Option<f32> {
    let nchars: Vec<char> = name.chars().collect();
    let qchars: Vec<char> = q.chars().collect();
    if qchars.is_empty() || qchars.len() > nchars.len() {
        return None;
    }
    // Forward: earliest end of a subsequence match.
    let mut qi = 0usize;
    let mut end = None;
    for (i, &c) in nchars.iter().enumerate() {
        if c == qchars[qi] {
            qi += 1;
            if qi == qchars.len() {
                end = Some(i);
                break;
            }
        }
    }
    let end = end?;
    // Backward from that end: latest start covering the query.
    let mut qj = qchars.len();
    let mut start = end;
    for i in (0..=end).rev() {
        if qj > 0 && nchars[i] == qchars[qj - 1] {
            qj -= 1;
            start = i;
            if qj == 0 {
                break;
            }
        }
    }
    let span = (end - start + 1) as f32;
    Some(qchars.len() as f32 / span)
}

/// Per-tier §5.13 UTF-16 ranges into the ORIGINAL name, computed only for
/// the final page. `fq` is the folded query.
fn compute_ranges(name: &str, fq: &str, tier: Tier) -> Vec<(u32, u32)> {
    match tier {
        Tier::Exact => vec![(0, utf16_len(name))],
        Tier::Prefix | Tier::Substring => {
            let (folded, map) = fold_with_map(name);
            let total = utf16_len(name);
            match memmem::find(folded.as_bytes(), fq.as_bytes()) {
                Some(p) => vec![(map[p], utf16_end(&map, total, p + fq.len() - 1))],
                None => Vec::new(),
            }
        }
        Tier::WordBoundary => {
            let (folded, map) = fold_with_map(name);
            let total = utf16_len(name);
            let bytes = folded.as_bytes();
            let mut first = None;
            let mut wb = None;
            for p in memmem::find_iter(bytes, fq.as_bytes()) {
                if first.is_none() {
                    first = Some(p);
                }
                if p > 0 && is_sep_byte(bytes[p - 1]) {
                    wb = Some(p);
                    break;
                }
            }
            let Some(p) = wb.or(first) else {
                return Vec::new();
            };
            // Highlight the matched segment: from the match start to the next
            // separator, extended if the match itself crosses separators.
            let seg_end = bytes[p..]
                .iter()
                .position(|&b| is_sep_byte(b))
                .map(|o| p + o)
                .unwrap_or(bytes.len());
            let end_byte = seg_end.max(p + fq.len());
            vec![(map[p], utf16_end(&map, total, end_byte - 1))]
        }
        Tier::Initials => initials_ranges(name, fq),
        Tier::Fuzzy => fuzzy_ranges(name, fq),
    }
}

/// Ranges for the initials tier: re-derive this one name's segment initials,
/// find the folded query inside them, and emit one per-char range per
/// matched segment-initial (merged when adjacent).
fn initials_ranges(name: &str, fq: &str) -> Vec<(u32, u32)> {
    let mut initials = String::new();
    // (initials_byte_off, initials_byte_len, utf16_start, utf16_len)
    let mut marks: Vec<(usize, usize, u32, u32)> = Vec::new();
    for (seg_start, _) in segment_spans(name) {
        let Some(c) = name[seg_start..].chars().next() else {
            continue;
        };
        let off = initials.len();
        for lc in c.to_lowercase() {
            initials.push(lc);
        }
        marks.push((
            off,
            initials.len() - off,
            utf16_offset_at_byte(name, seg_start),
            c.len_utf16() as u32,
        ));
    }
    let Some(pos) = memmem::find(initials.as_bytes(), fq.as_bytes()) else {
        return Vec::new();
    };
    let hit_start = pos;
    let hit_end = pos + fq.len();
    let mut ranges = Vec::new();
    for &(off, len, u16s, u16l) in &marks {
        if off < hit_end && off + len > hit_start {
            ranges.push((u16s, u16s + u16l));
        }
    }
    merge_ranges(ranges)
}

/// Ranges for the fuzzy tier: greedy forward subsequence over the folded
/// name, one range per matched char, merged when adjacent.
fn fuzzy_ranges(name: &str, fq: &str) -> Vec<(u32, u32)> {
    let (folded, map) = fold_with_map(name);
    let total = utf16_len(name);
    let qchars: Vec<char> = fq.chars().collect();
    let mut qi = 0usize;
    let mut ranges = Vec::new();
    for (bi, c) in folded.char_indices() {
        if qi < qchars.len() && c == qchars[qi] {
            qi += 1;
            let s = map[bi];
            let e = utf16_end(&map, total, bi + c.len_utf8() - 1);
            ranges.push((s, e));
        }
    }
    merge_ranges(ranges)
}

/// Ranked heap element; `Ord` = better (higher score, then lower entry idx).
struct Scored {
    score: f32,
    eidx: u32,
    tier: Tier,
}

impl PartialEq for Scored {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Scored {}
impl PartialOrd for Scored {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Scored {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .total_cmp(&other.score)
            .then_with(|| other.eidx.cmp(&self.eidx))
    }
}

/// Tiered search over one volume index (§3.4); called from
/// [`VolumeIndex::search`]. Returns up to `max_results` hits, best first.
pub(crate) fn search(
    ix: &VolumeIndex,
    query: &str,
    max_results: usize,
    is_cancelled: &dyn Fn() -> bool,
) -> Vec<Hit> {
    let fq = fold(query);
    if fq.is_empty() || max_results == 0 || ix.entries.is_empty() {
        return Vec::new();
    }

    let mut accel = ix.accel_lock();
    accel.ensure_basic(ix);

    let mut best: HashMap<u32, Cand> = HashMap::new();
    let finder = memmem::Finder::new(fq.as_bytes());
    let mut processed = 0usize;
    let mut cancelled = is_cancelled();

    // Pass 1: one scan of the folded name arena classifies the
    // exact / prefix / word-boundary / substring tiers per hit.
    if !cancelled {
        let arena = ix.folded_arena.as_bytes();
        for hit in finder.find_iter(arena) {
            processed += 1;
            if processed % CANCEL_STRIDE == 0 && is_cancelled() {
                cancelled = true;
                break;
            }
            let Some(eidx) = entry_at(&accel.folded_order, ix, hit, fq.len()) else {
                continue;
            };
            let e = &ix.entries[eidx as usize];
            let tier = if hit == e.folded_off as usize {
                if fq.len() == e.folded_len as usize {
                    Tier::Exact
                } else {
                    Tier::Prefix
                }
            } else if is_sep_byte(arena[hit - 1]) {
                Tier::WordBoundary
            } else {
                Tier::Substring
            };
            upgrade(&mut best, eidx, tier, tier_base(tier));
        }
    }

    // Pass 2: camel/initials — substring scan of the initials arena.
    if !cancelled {
        for hit in finder.find_iter(accel.initials_arena.as_bytes()) {
            processed += 1;
            if processed % CANCEL_STRIDE == 0 && is_cancelled() {
                cancelled = true;
                break;
            }
            let Some(eidx) = initials_entry_at(&accel.initials_spans, hit, fq.len()) else {
                continue;
            };
            upgrade(&mut best, eidx, Tier::Initials, tier_base(Tier::Initials));
        }
    }

    // Pass 3: fuzzy subsequence over trigram-intersection candidates.
    // Queries shorter than 3 bytes skip the tier entirely (§3.4).
    if !cancelled && fq.len() >= 3 {
        accel.ensure_trigrams(ix);
        let tg = accel.trigrams.as_ref().expect("just built");
        let qb = fq.as_bytes();
        let mut lists: Vec<&[u32]> = Vec::with_capacity(qb.len() - 2);
        let mut all_present = true;
        for w in qb.windows(3) {
            match tg.get(&[w[0], w[1], w[2]]) {
                Some(l) => lists.push(l.as_slice()),
                None => {
                    all_present = false;
                    break;
                }
            }
        }
        if all_present && !lists.is_empty() {
            lists.sort_unstable_by_key(|l| l.len());
            let (first, rest) = lists.split_first().expect("non-empty");
            let mut scored = 0usize;
            for &cand in first.iter() {
                if best.contains_key(&cand) {
                    continue; // already matched by a higher tier (fuzzy max 0.5 < 0.55)
                }
                if !rest.iter().all(|l| l.binary_search(&cand).is_ok()) {
                    continue;
                }
                scored += 1;
                if scored > FUZZY_CAP {
                    break;
                }
                if scored % CANCEL_STRIDE == 0 && is_cancelled() {
                    cancelled = true;
                    break;
                }
                let e = &ix.entries[cand as usize];
                if let Some(density) = fuzzy_density(ix.folded_of_entry(e), &fq) {
                    upgrade(&mut best, cand, Tier::Fuzzy, 0.3 + 0.2 * density);
                }
            }
        }
    }
    if cancelled {
        log::trace!("search cancelled early; returning partial results");
    }

    // Rank: score = base × depth_penalty × hidden_penalty; top-K via heap.
    let mut heap: BinaryHeap<Reverse<Scored>> = BinaryHeap::new();
    for (i, (&eidx, cand)) in best.iter().enumerate() {
        if (i + 1) % CANCEL_STRIDE == 0 && is_cancelled() {
            break; // return what we have (§3.4 cancellation)
        }
        let e = &ix.entries[eidx as usize];
        let depth = ix.depth_of(eidx);
        let mut score = cand.base / (1.0 + 0.02 * depth as f32);
        if e.flags & (flags::HIDDEN | flags::SYSTEM) != 0 {
            score *= 0.85;
        }
        heap.push(Reverse(Scored {
            score,
            eidx,
            tier: cand.tier,
        }));
        if heap.len() > max_results {
            heap.pop();
        }
    }
    let mut top: Vec<Scored> = heap.into_iter().map(|r| r.0).collect();
    top.sort_by(|a, b| b.cmp(a));

    // match_ranges only for the final page (§3.4).
    let mut hits = Vec::with_capacity(top.len());
    for s in &top {
        let e = &ix.entries[s.eidx as usize];
        let name = ix.name_of_entry(e);
        hits.push(Hit {
            frn: e.frn,
            score: s.score,
            match_ranges: compute_ranges(name, &fq, s.tier),
        });
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EntrySink;

    fn no_cancel() -> bool {
        false
    }

    fn ix() -> VolumeIndex {
        VolumeIndex::new(0, "C:\\".to_string())
    }

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    #[test]
    fn tier_ordering() {
        let mut v = ix();
        v.add(1, 999, "foo", 0); // exact          1.0
        v.add(2, 999, "foobar", 0); // prefix         0.9
        v.add(3, 999, "my-foo.txt", 0); // word boundary  0.8
        v.add(4, 999, "fat old otters", 0); // initials "foo" 0.7
        v.add(5, 999, "xfoox", 0); // substring      0.55
        let hits = v.search("foo", 10, &no_cancel);
        let frns: Vec<u64> = hits.iter().map(|h| h.frn).collect();
        assert_eq!(frns, vec![1, 2, 3, 4, 5]);
        assert!(approx(hits[0].score, 1.0));
        assert!(approx(hits[1].score, 0.9));
        assert!(approx(hits[2].score, 0.8));
        assert!(approx(hits[3].score, 0.7));
        assert!(approx(hits[4].score, 0.55));
    }

    #[test]
    fn exact_is_case_and_nfc_insensitive() {
        let mut v = ix();
        v.add(1, 999, "README.md", 0);
        let hits = v.search("readme.md", 10, &no_cancel);
        assert_eq!(hits.len(), 1);
        assert!(approx(hits[0].score, 1.0));
        assert_eq!(hits[0].match_ranges, vec![(0, 9)]);
    }

    #[test]
    fn depth_penalty_prefers_shallow() {
        let mut v = ix();
        v.add(1, 999, "target.txt", 0); // depth 0
        v.add(10, 999, "a", crate::flags::DIR);
        v.add(11, 10, "b", crate::flags::DIR);
        v.add(12, 11, "target.txt", 0); // depth 2
        let hits = v.search("target", 10, &no_cancel);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].frn, 1);
        assert_eq!(hits[1].frn, 12);
        assert!(approx(hits[0].score, 0.9));
        assert!(approx(hits[1].score, 0.9 / 1.04));
    }

    #[test]
    fn hidden_ranks_below_normal() {
        let mut v = ix();
        v.add(1, 999, "app.exe", 0);
        v.add(2, 999, "app.exe", crate::flags::HIDDEN);
        v.add(3, 999, "app.exe", crate::flags::SYSTEM);
        let hits = v.search("app", 10, &no_cancel);
        assert_eq!(hits[0].frn, 1);
        assert!(approx(hits[0].score, 0.9));
        assert!(approx(hits[1].score, 0.9 * 0.85));
        assert!(approx(hits[2].score, 0.9 * 0.85));
    }

    #[test]
    fn non_ascii_prefix_nfd_name_nfc_query() {
        let mut v = ix();
        v.add(1, 999, "Cafe\u{301}.txt", 0); // NFD on disk
        let hits = v.search("CAF\u{c9}", 10, &no_cancel); // NFC upper-case query
        assert_eq!(hits.len(), 1);
        assert!(approx(hits[0].score, 0.9)); // prefix
        assert_eq!(hits[0].match_ranges, vec![(0, 4)]); // C,a,f,é in UTF-16
    }

    #[test]
    fn cjk_substring() {
        let mut v = ix();
        v.add(1, 999, "汉语手册.pdf", 0);
        let hits = v.search("语手", 10, &no_cancel);
        assert_eq!(hits.len(), 1);
        assert!(approx(hits[0].score, 0.55));
        assert_eq!(hits[0].match_ranges, vec![(1, 3)]);
        let hits = v.search("汉语", 10, &no_cancel);
        assert!(approx(hits[0].score, 0.9)); // prefix
        assert_eq!(hits[0].match_ranges, vec![(0, 2)]);
    }

    #[test]
    fn astral_plane_utf16_ranges() {
        let mut v = ix();
        v.add(1, 999, "\u{1d11e}abc.txt", 0); // 𝄞 = 2 UTF-16 units
        let hits = v.search("abc", 10, &no_cancel);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].match_ranges, vec![(2, 5)]); // after the surrogate pair
        let hits = v.search("\u{1d11e}", 10, &no_cancel);
        assert!(approx(hits[0].score, 0.9)); // prefix
        assert_eq!(hits[0].match_ranges, vec![(0, 2)]); // both units of 𝄞
    }

    #[test]
    fn word_boundary_highlights_matched_segment() {
        let mut v = ix();
        v.add(1, 999, "foo-barbaz.txt", 0);
        let hits = v.search("ba", 10, &no_cancel);
        assert_eq!(hits.len(), 1);
        assert!(approx(hits[0].score, 0.8));
        // Whole segment "barbaz" is highlighted.
        assert_eq!(hits[0].match_ranges, vec![(4, 10)]);
    }

    #[test]
    fn initials_camel_case() {
        let mut v = ix();
        v.add(1, 999, "FooBar.txt", 0);
        let hits = v.search("fb", 10, &no_cancel);
        assert_eq!(hits.len(), 1);
        assert!(approx(hits[0].score, 0.7));
        // 'F' of Foo and 'B' of Bar; not adjacent so two ranges.
        assert_eq!(hits[0].match_ranges, vec![(0, 1), (3, 4)]);
    }

    #[test]
    fn fuzzy_subsequence_density_and_ranges() {
        let mut v = ix();
        v.add(1, 999, "abcx_bcd", 0);
        let hits = v.search("abcd", 10, &no_cancel);
        assert_eq!(hits.len(), 1);
        // density = 4 matched / span 8 = 0.5 → 0.3 + 0.2·0.5 = 0.4
        assert!(approx(hits[0].score, 0.4));
        // a,b,c adjacent (merged) then d.
        assert_eq!(hits[0].match_ranges, vec![(0, 3), (7, 8)]);
    }

    #[test]
    fn fuzzy_skipped_below_three_bytes() {
        let mut v = ix();
        v.add(1, 999, "axbx", 0);
        // "ab" is a subsequence of "axbx", but 2-byte queries skip fuzzy and
        // nothing else matches.
        assert!(v.search("ab", 10, &no_cancel).is_empty());
    }

    #[test]
    fn dedup_across_tiers_keeps_best() {
        let mut v = ix();
        v.add(1, 999, "foofoo", 0); // prefix hit at 0 and substring hit at 3
        let hits = v.search("foo", 10, &no_cancel);
        assert_eq!(hits.len(), 1);
        assert!(approx(hits[0].score, 0.9)); // prefix wins
        assert_eq!(hits[0].match_ranges, vec![(0, 3)]);
    }

    #[test]
    fn max_results_caps_and_orders() {
        let mut v = ix();
        for i in 0..9u64 {
            v.add(i + 1, 999, &format!("aaa{i}.txt"), 0);
        }
        let hits = v.search("aaa", 5, &no_cancel);
        assert_eq!(hits.len(), 5);
        for w in hits.windows(2) {
            assert!(w[0].score >= w[1].score);
        }
    }

    #[test]
    fn cancellation_returns_partial() {
        let mut v = ix();
        for i in 0..100u64 {
            v.add(i + 1, 999, &format!("file{i}.txt"), 0);
        }
        // Cancelled before any pass: nothing collected, returns cleanly.
        let hits = v.search("file", 10, &|| true);
        assert!(hits.is_empty());
        // Never cancelled: full results.
        let hits = v.search("file", 10, &no_cancel);
        assert_eq!(hits.len(), 10);
    }

    #[test]
    fn no_cross_entry_arena_matches() {
        let mut v = ix();
        // Adjacent in the folded arena: "abc" + "def" — query "cd" must not
        // match across the boundary.
        v.add(1, 999, "abc", 0);
        v.add(2, 999, "def", 0);
        assert!(v.search("cd", 10, &no_cancel).is_empty());
    }

    #[test]
    fn segment_spans_camel_and_separators() {
        assert_eq!(segment_spans("FooBar.txt"), vec![(0, 3), (3, 6), (7, 10)]);
        assert_eq!(segment_spans("my-file.txt"), vec![(0, 2), (3, 7), (8, 11)]);
        assert_eq!(segment_spans("..."), Vec::<(usize, usize)>::new());
        assert_eq!(segment_spans("a b"), vec![(0, 1), (2, 3)]);
    }

    #[test]
    fn fold_with_map_offsets() {
        // 'A' → utf16 0; '𝄞' (4 UTF-8 bytes) → utf16 1; 'b' → utf16 3.
        let (folded, map) = fold_with_map("A\u{1d11e}b");
        assert_eq!(folded, "a\u{1d11e}b");
        assert_eq!(map, vec![0, 1, 1, 1, 1, 3]);
        assert_eq!(utf16_end(&map, 4, 0), 1); // end of 'a'
        assert_eq!(utf16_end(&map, 4, 4), 3); // end of '𝄞'
        assert_eq!(utf16_end(&map, 4, 5), 4); // end of 'b' (total)
    }

    #[test]
    fn fold_with_map_matches_fold_on_nfc_input() {
        for s in ["README.md", "Caf\u{e9}.txt", "汉语手册.pdf", "FooBar"] {
            let (folded, map) = fold_with_map(s);
            assert_eq!(folded, fold(s));
            assert_eq!(map.len(), folded.len());
        }
    }

    #[test]
    fn merge_ranges_merges_touching() {
        assert_eq!(
            merge_ranges(vec![(3, 4), (0, 1), (1, 2)]),
            vec![(0, 2), (3, 4)]
        );
        assert_eq!(merge_ranges(vec![(0, 2), (1, 3)]), vec![(0, 3)]);
    }

    #[test]
    fn fuzzy_density_basics() {
        assert!(approx(fuzzy_density("abcd", "abcd").unwrap(), 1.0));
        assert!(approx(fuzzy_density("abcx_bcd", "abcd").unwrap(), 0.5));
        assert_eq!(fuzzy_density("abc", "abd"), None);
        assert_eq!(fuzzy_density("ab", "abc"), None);
    }

    #[test]
    fn rebuild_after_delete_keeps_matches_consistent() {
        let mut v = ix();
        v.add(1, 999, "alpha.txt", 0);
        v.add(2, 999, "beta.txt", 0);
        v.add(3, 999, "gamma.txt", 0);
        assert_eq!(v.search("beta", 10, &no_cancel).len(), 1);
        v.apply(crate::UsnEvent::Delete { frn: 2 });
        // Stale "beta.txt" bytes remain in the arena but must not match.
        assert!(v.search("beta", 10, &no_cancel).is_empty());
        assert_eq!(v.search("gamma", 10, &no_cancel)[0].frn, 3);
    }
}
