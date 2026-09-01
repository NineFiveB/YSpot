//! Tiered matcher and candidate prefilters (SPEC §3.4).
//!
//! Tier order (base match quality):
//! exact `1.0` > prefix `0.9` > word-boundary segment start `0.8` >
//! camel/initials `0.7` > contiguous substring `0.55` > fuzzy subsequence
//! `0.3..=0.5` (by density `matched_len / span`).
//!
//! Final score = `base × depth_penalty × hidden_penalty` where
//! `depth_penalty = 1 / (1 + 0.02·path_depth)` and entries carrying the
//! HIDDEN or SYSTEM attribute are additionally multiplied by `0.85`. Both
//! factors are read out of the index's 1 B/entry `rank_key` column through
//! [`crate::index::DEPTH_PEN`] — a table lookup, not a parent-chain walk.
//!
//! Candidates are ranked INLINE, into a min-heap capped at `2·max_results`
//! (see [`Selector`]), so a pass whose best possible score cannot reach the
//! page is skipped and a hit whose tier cannot reach it is rejected from the
//! two arena bytes fencing it, before anything is mapped or looked up. Every
//! such comparison is strict; the reason, and the test that holds it in place,
//! are on [`Selector`].
//!
//! Candidate generation never walks the whole entry table per tier:
//! exact/prefix/word-boundary/substring fall out of one `memmem` scan over
//! the folded name arena — which is `0x00 rec 0x00 rec 0x00`, so the two
//! bytes fencing a hit give its tier outright and a delimiter-free query can
//! only ever match inside a single record (hits are mapped to slots in O(1)
//! through the index's `owner`/`arena_recs` pair, see [`slot_at`]);
//! initials are scanned in the index's fixed-stride [`INITIALS_STRIDE`]-byte
//! column, where a hit maps to its slot by a divide and needs no table at all;
//! fuzzy candidates come from byte-trigram posting-list intersection, capped at
//! [`FUZZY_CAP`] scored candidates per query.
//!
//! `match_ranges` are UTF-16 code-unit ranges into the ORIGINAL (NFC) name
//! (§5.13), computed only for the final top-K page via [`fold_with_map`].

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

use memchr::memmem;

use crate::index::{
    class_mask, fold, has_control_byte, ArenaRec, Entry, VolumeIndex, BSI_CLASSES, DEPTH_PEN,
    FOLDED_DELIM, OWNER_SHIFT, RANK_DEPTH_MAX,
};
use crate::Hit;

/// Cancellation is polled at least every this many hits/candidates (§3.4
/// mandates prompt cancel on new keystrokes) and before every pass.
const CANCEL_STRIDE: usize = 4096;

/// Pass A also polls cancellation every this many arena bytes advanced, so a
/// query that hits nothing at all is still interruptible: `CANCEL_STRIDE` is
/// counted in HITS, and a zero-hit scan of a 24 MB arena would otherwise run to
/// completion no matter how many keystrokes arrived.
const SCAN_POLL_CHUNK: usize = 1 << 20;

/// Max fuzzy candidates scored per query (§3.4 prefilter bound).
const FUZZY_CAP: usize = 20_000;

fn is_sep_byte(b: u8) -> bool {
    matches!(b, b'-' | b'_' | b'.' | b' ')
}

fn is_sep_char(c: char) -> bool {
    matches!(c, '-' | '_' | '.' | ' ')
}

/// Longest folded query the fuzzy pass verifies from a stack buffer. Past it
/// the query chars are collected once per SEARCH — never per candidate, which
/// is the whole point of [`fuzzy_density`]'s signature.
const FUZZY_QBUF: usize = 64;

// Counts of Pass A arena scans and Pass B initials-column scans actually
// started, for the tests that assert a presence-set miss returns without
// touching the column. Thread-local, so the test harness's parallel threads
// cannot see each other's scans.
#[cfg(test)]
thread_local! {
    static ARENA_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static INITIALS_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_arena_scan() {
    ARENA_SCANS.with(|c| c.set(c.get() + 1));
}

#[cfg(not(test))]
#[inline(always)]
fn note_arena_scan() {}

#[cfg(test)]
fn note_initials_scan() {
    INITIALS_SCANS.with(|c| c.set(c.get() + 1));
}

#[cfg(not(test))]
#[inline(always)]
fn note_initials_scan() {}

/// Count folded-arena hits for `needle` — Pass 1's `memmem` scan with nothing
/// attached to it: no hit→entry mapping, no tier classification, no ranking.
///
/// Instrumentation only (bench §10 M0). It isolates the irreducible scan term
/// every query pays, which is what SPEC §3.4's "a 1M-name arena scans in
/// single-digit ms" premise rests on; a probe needle that matches nothing
/// forces the scan to run to the end of the arena. The fold is done here so
/// the caller passes the same string it would pass to [`search`]; at probe
/// needle lengths it is far below the scan cost.
pub(crate) fn arena_scan_probe(ix: &VolumeIndex, needle: &str) -> usize {
    let fq = fold(needle);
    // Rejected on the same rule as a real query: a needle carrying a reserved
    // byte would count record fences instead of names, which is not the scan
    // this probe is meant to isolate.
    if fq.is_empty() || has_control_byte(&fq) {
        return 0;
    }
    memmem::find_iter(ix.folded_arena.as_bytes(), fq.as_bytes()).count()
}

/// Width of one slot's lane in the index's initials column, in bytes.
///
/// A slot's folded segment initials occupy
/// `initials[INITIALS_STRIDE·slot .. INITIALS_STRIDE·(slot+1)]`, NUL-padded, so
/// a hit maps to its slot by one divide and its containment test is one
/// remainder — no span table, no `partition_point`, and a flat 8 B/entry in
/// place of a packed arena plus a 12 B/entry `(off, len, slot)` record.
///
/// The price is behavior change 2: initials past the eighth byte are LOST and a
/// query wider than a lane cannot reach the tier at all (see
/// [`initials_lane`]). Real names carry 2-4 segments; widening the tier is this
/// one constant and 4 more B/entry.
pub(crate) const INITIALS_STRIDE: usize = 8;

/// Visit each segment of the ORIGINAL (NFC, original-case) `name`: maximal runs
/// of non-separator chars (`-`, `_`, `.`, space), additionally split at
/// lower→upper camel transitions (`FooBar` → `Foo`, `Bar`).
///
/// Allocation-free, and the single definition of the segmentation rule: `f`
/// receives each segment's START (which is all the initials column and
/// [`initials_ranges`] want) together with its end, so the allocating
/// [`segment_spans`] is a wrapper over this same walk rather than a second copy
/// of it. Returning `false` from `f` stops the walk — that is what lets a full
/// initials lane abandon the rest of a long name instead of segmenting it.
pub(crate) fn for_each_segment_start(name: &str, mut f: impl FnMut(usize, usize) -> bool) {
    let mut start: Option<usize> = None;
    let mut prev: Option<char> = None;
    for (i, c) in name.char_indices() {
        if is_sep_char(c) {
            if let Some(s) = start.take() {
                if !f(s, i) {
                    return;
                }
            }
        } else {
            let camel = matches!(prev, Some(p) if p.is_lowercase() && c.is_uppercase());
            if camel {
                if let Some(s) = start.take() {
                    if !f(s, i) {
                        return;
                    }
                }
                start = Some(i);
            } else if start.is_none() {
                start = Some(i);
            }
        }
        prev = Some(c);
    }
    if let Some(s) = start {
        f(s, name.len());
    }
}

/// Byte spans of `name`'s segments, collected. The allocating wrapper over
/// [`for_each_segment_start`], kept for the final-page [`initials_ranges`] —
/// which needs every segment's UTF-16 offset anyway — and for the tests that
/// pin the segmentation rule down.
pub(crate) fn segment_spans(name: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    for_each_segment_start(name, |s, e| {
        spans.push((s, e));
        true
    });
    spans
}

/// One slot's lane in the initials column: the folded first char of each of
/// `name`'s segments, packed and NUL-padded to [`INITIALS_STRIDE`].
///
/// Allocation-free — this runs on the mutation path for every create, rename
/// and same-FRN re-create.
///
/// **Truncation is per INITIAL, and it stops the walk.** An initial whose
/// folded form does not fit is dropped whole and nothing after it is
/// considered. Writing a partial UTF-8 sequence would leave bytes in the lane
/// that spell no character; skipping a wide initial to fit a later narrow one
/// would put initials in an order the name does not have, and a query matching
/// that order would be a wrong result rather than a missing one. A missing
/// result is the failure this tier is allowed to have (behavior change 2).
pub(crate) fn initials_lane(name: &str) -> [u8; INITIALS_STRIDE] {
    // Padded with the same byte the folded arena fences records with, for the
    // same reason: invariant I3 keeps it out of every folded query, so padding
    // is inert and a short lane cannot be matched into.
    let mut lane = [FOLDED_DELIM; INITIALS_STRIDE];
    let mut len = 0usize;
    for_each_segment_start(name, |start, _| {
        let Some(c) = name[start..].chars().next() else {
            return true;
        };
        // `char::to_lowercase` is the same fold `fold` applies, and it can
        // expand one char into several (U+0130 → `i` + U+0307), so the initial
        // is staged whole before any of it is committed to the lane. The buffer
        // is that iterator's own bound: at most 3 chars of at most 4 bytes.
        let mut buf = [0u8; 3 * 4];
        let mut n = 0usize;
        for lc in c.to_lowercase() {
            let need = lc.len_utf8();
            if n + need > buf.len() {
                return false;
            }
            lc.encode_utf8(&mut buf[n..n + need]);
            n += need;
        }
        if len + n > INITIALS_STRIDE {
            return false;
        }
        lane[len..len + n].copy_from_slice(&buf[..n]);
        len += n;
        true
    });
    lane
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

/// Exact upper bound on any final score a tier can produce.
///
/// `score = base × depth_penalty × hidden_penalty` and both factors are ≤ 1, so
/// the tier's own base bounds it. Nothing else about the candidate is needed,
/// which is the point: this is testable from the two arena bytes fencing a hit,
/// before any mapping, entry read or column touch.
///
/// Distinct from [`tier_base`] for exactly one tier. `tier_base(Fuzzy)` is 0.3,
/// documented at its definition as a FLOOR — the real fuzzy base is
/// `0.3 + 0.2·density`, up to 0.5. Using 0.3 as a bound would discard every
/// fuzzy candidate scoring 0.31-0.5, silently, with no test failing.
fn tier_upper_bound(t: Tier) -> f32 {
    match t {
        Tier::Exact => 1.0,
        Tier::Prefix => 0.9,
        Tier::WordBoundary => 0.8,
        Tier::Initials => 0.7,
        Tier::Substring => 0.55,
        Tier::Fuzzy => 0.5,
    }
}

/// Tier of a folded-arena hit, from the two bytes fencing it — both already in
/// L1 from the scan, so this is a couple of compares and no lookup at all.
///
/// Sound because the arena is `0x00 rec 0x00 rec 0x00` and the query is
/// delimiter-free (checked in [`search`]): a hit therefore lies strictly inside
/// one record and has a fence byte on each side. A leading delimiter means the
/// match starts the name, a trailing one means it ends the name, both means the
/// match IS the name.
#[inline]
fn classify(before: u8, after: u8) -> Tier {
    match (before, after) {
        (FOLDED_DELIM, FOLDED_DELIM) => Tier::Exact,
        (FOLDED_DELIM, _) => Tier::Prefix,
        (b, _) if is_sep_byte(b) => Tier::WordBoundary,
        _ => Tier::Substring,
    }
}

/// Map a folded-arena hit offset to the slot whose record contains it, or
/// `None` if that record has been superseded or its slot tombstoned.
///
/// **O(1), no search.** `owner[hit >> OWNER_SHIFT]` names the last record
/// starting at or before the 64-byte block the hit lies in, so the walk that
/// follows is bounded by how many records start inside one block — ~2.7 at the
/// §3.4 accounting's 23 B mean name. Both arrays are read in ascending order
/// across a scan, so they stream rather than missing once per hit.
///
/// What it replaces is a named contributor to the 1M latency: a
/// `partition_point` over a sorted 8 MB `(folded_off, slot)` vec — ~20 random
/// probes per raw hit — that also had to be rebuilt and re-sorted on the first
/// query after any mutation.
///
/// **Why the record is checked against the entry.** `arena_recs` is
/// append-only, so a renamed slot leaves its old record behind pointing at a
/// byte range that no longer describes it — and, once the freelist hands that
/// slot to a new file, describes a DIFFERENT file. A delete leaves one pointing
/// at a tombstone. `unregister_slot` NUL-filled both ranges (invariant I4), so
/// a delimiter-free needle cannot actually match inside them; the two tests
/// here are the belt to that pair of braces, and what keeps this function
/// honest if a later step relaxes the erase or leaves a record behind at
/// compaction. `folded_off` and `flags` are fields of the same 32 B `Entry`, so
/// together they cost the one read the containment `debug_assert` needs anyway.
#[inline]
fn slot_at(
    owner: &[u32],
    recs: &[ArenaRec],
    entries: &[Entry],
    hit: usize,
    qlen: usize,
) -> Option<u32> {
    let mut ri = owner[hit >> OWNER_SHIFT] as usize;
    while ri + 1 < recs.len() && (recs[ri + 1].off as usize) <= hit {
        ri += 1;
    }
    let rec = recs[ri];
    let e = &entries[rec.slot as usize];
    if rec.off != e.folded_off || e.is_dead() {
        return None;
    }
    // Live record + fenced arena + delimiter-free needle ⇒ the whole match is
    // inside this record. Asserted rather than tested: a violation means the
    // arena lost a fence, and a silently dropped hit would hide that.
    debug_assert!(
        hit + qlen <= rec.off as usize + e.folded_len as usize,
        "hit at {hit} escaped its record: arena fencing"
    );
    Some(rec.slot)
}

/// Visit every slot whose character-class set is a SUPERSET of `qmask`, in
/// ascending slot order, stopping early if `f` returns `false`.
///
/// This is the fuzzy tier's whole candidate generator. It ANDs one bit-slice
/// per class the query needs — 4-8 of them for a real query, ~1 MB of
/// sequential reads at 1M entries — and pulls survivors out of the result with
/// `trailing_zeros`. What it replaces is a `HashMap<[u8;3], Vec<u32>>` of
/// posting lists: 96 B/entry resident, rebuilt from a full sweep of the entry
/// table after every mutation, intersected by binary search, and UNSOUND as a
/// subsequence prefilter (a gapped query carries none of its own trigrams).
///
/// A dead slot cannot survive: `unregister_slot` clears its bits, and a
/// non-empty query names at least one class. Slots past the entry table cannot
/// either — no bit is ever set for them.
fn for_each_class_superset(
    bsi: &[u64],
    words_per_slice: usize,
    words: usize,
    qmask: u64,
    mut f: impl FnMut(u32) -> bool,
) {
    // The class indices, once, so the inner loop is a walk over a short slice
    // rather than a bit-scan per word.
    let mut classes = [0u8; BSI_CLASSES];
    let mut n = 0usize;
    let mut m = qmask;
    while m != 0 {
        classes[n] = m.trailing_zeros() as u8;
        m &= m - 1;
        n += 1;
    }
    let classes = &classes[..n];
    for w in 0..words {
        let mut acc = u64::MAX;
        for &c in classes {
            acc &= bsi[c as usize * words_per_slice + w];
            if acc == 0 {
                break;
            }
        }
        while acc != 0 {
            let b = acc.trailing_zeros();
            acc &= acc - 1;
            if !f((w * 64) as u32 + b) {
                return;
            }
        }
    }
}

/// Subsequence match density of the folded query `q` in the folded `name`:
/// `matched_len / span` in chars, using the earliest-end window tightened
/// backward (bounded two-pass, O(|name|)). `None` if `q` is not a subsequence
/// of `name`.
///
/// **Allocation-free, and that is the reason for the `&[char]` query.** This is
/// the fuzzy tier's verifier: it runs once per surviving candidate, up to
/// [`FUZZY_CAP`] times per query. It used to `collect()` the name AND the query
/// into a `Vec<char>` on every call — 40,000 allocations per query at the cap,
/// paid to re-derive the same query each time. The query is now collected once
/// per search by the caller (into a stack buffer where it fits) and the name is
/// walked through `char_indices` in place.
///
/// The backward pass indexes chars while iterating bytes, so it tracks the char
/// index down alongside `char_indices().rev()`; `span` is in CHARS either way,
/// which is what keeps the density identical to the collecting version.
fn fuzzy_density(name: &str, q: &[char]) -> Option<f32> {
    if q.is_empty() {
        return None;
    }
    // Forward: earliest end of a subsequence match.
    let mut qi = 0usize;
    let mut hit: Option<(usize, usize)> = None; // (char index, byte end)
    for (ci, (bi, c)) in name.char_indices().enumerate() {
        if c == q[qi] {
            qi += 1;
            if qi == q.len() {
                hit = Some((ci, bi + c.len_utf8()));
                break;
            }
        }
    }
    let (end_char, end_byte) = hit?;
    // Backward from that end: latest start covering the query.
    let mut qj = q.len();
    let mut start_char = end_char;
    let mut idx = end_char;
    for (_, c) in name[..end_byte].char_indices().rev() {
        if qj > 0 && c == q[qj - 1] {
            qj -= 1;
            start_char = idx;
            if qj == 0 {
                break;
            }
        }
        idx = idx.wrapping_sub(1);
    }
    let span = (end_char - start_char + 1) as f32;
    Some(q.len() as f32 / span)
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

/// Running top-K: a min-heap capped at `2·max_results`, plus a `seen` bitset so
/// the fuzzy pass can skip slots a higher tier already claimed.
///
/// This replaces a `HashMap<u32, Cand>` that collected EVERY candidate and a
/// ranking loop that then walked all of them calling `depth_of`. Candidates
/// are now scored and admitted inline, so the pruning rules below can look at a
/// live floor and stop work that provably cannot reach the page.
///
/// **Why 2K is exactly enough.** An entry is offered at most twice: once by
/// Pass A (all its hits lie inside its one contiguous record, and consecutive-
/// slot grouping collapses them into a single offer) and once by Pass B; Pass C
/// skips `seen` slots. Let `s_K` be the K-th best distinct slot, with best
/// offer `(v, s_K)`. Each of the K-1 better distinct slots contributes at most
/// 2 offers, so `(v, s_K)` ranks at worst 2K-1 among all offers and a 2K heap
/// retains it. Drain, dedup by slot keeping the max, sort, truncate to K —
/// identical to ranking everything, with no heap-position bookkeeping.
///
/// **Why the rules are STRICT.** [`Scored::cmp`] breaks equal scores by LOWER
/// `eidx`, so an offer whose score merely EQUALS the floor can still displace
/// the current 2K-th. `≥`/`≤` rules would discard those tie-break winners —
/// a wrong-results bug that changes nothing an existing test asserts, which is
/// why the differential test in this module's tests exists.
struct Selector<'a> {
    heap: BinaryHeap<Reverse<Scored>>,
    /// Caller-supplied acceptance test on a slot, consulted only for an offer
    /// that would otherwise be ADMITTED — see [`Selector::offer`]. Pushing a
    /// filter in here rather than discarding rows afterwards is what lets a
    /// filtered query fetch exactly `max_results`: post-filtering has to
    /// over-fetch a guessed multiple and still silently under-returns when the
    /// filter is selective enough that the guess was wrong.
    accept: &'a dyn Fn(u32) -> bool,
    /// `2·max_results`.
    cap: usize,
    /// Score of the current 2K-th best offer. Meaningful only once the heap is
    /// full; monotonically non-decreasing from then on, which is what makes the
    /// rules conservative — the running floor is the 2K-th best over a PREFIX
    /// of the offer stream, hence never above the final 2K-th best.
    floor: f32,
    /// One bit per slot: offered by some pass already.
    seen: Vec<u64>,
}

impl<'a> Selector<'a> {
    fn new(max_results: usize, slots: usize, accept: &'a dyn Fn(u32) -> bool) -> Self {
        let cap = max_results.saturating_mul(2).max(1);
        Self {
            accept,
            heap: BinaryHeap::with_capacity(cap),
            cap,
            floor: f32::NEG_INFINITY,
            seen: vec![0u64; slots.div_ceil(64)],
        }
    }

    fn full(&self) -> bool {
        self.heap.len() >= self.cap
    }

    /// Whether a candidate bounded by `ub` provably cannot reach the page.
    ///
    /// STRICT `<`: on equality the offer falls through to the full
    /// [`Scored::cmp`], which is where the `eidx` tie-break lives. Used both as
    /// the per-hit reject (~5 ns, touches no memory but the two fence bytes)
    /// and, with the tier's own bound, as the whole-pass skip.
    fn rejects(&self, ub: f32) -> bool {
        self.full() && ub < self.floor
    }

    fn is_seen(&self, slot: u32) -> bool {
        self.seen[slot as usize >> 6] >> (slot & 63) & 1 != 0
    }

    /// Score `slot` and admit it. `rank_key` supplies depth and the
    /// hidden/system penalty in one byte — no entry read, no parent walk.
    ///
    /// **The filter is consulted LAST, and only for an offer that has already
    /// beaten the heap.** Scoring is a `rank_key` byte and a table lookup, both
    /// sequential; `accept` is a closure the caller supplies over the NAME, and
    /// the one `search` builds (`accept(ix.name_of_entry(&ix.entries[slot]))`)
    /// costs two dependent RANDOM reads — into the 32 MB entry table, then into
    /// the 24 MB name arena — to materialize a `&str`. Testing it first meant
    /// paying that for every candidate a pass produced, including the ~20,000 a
    /// 2-char initials query offers, and including the unfiltered case where
    /// `accept` is the `|_| true` [`crate::VolumeIndex::search`] passes: 19,950
    /// calls for one query, ~215 ns each, ~75% of that pass's cost spent
    /// building strings for a closure that ignores them.
    ///
    /// Result-preserving, which is the whole reason it is safe to move: the
    /// admission tests below decide membership on `(score, eidx, tier)` alone,
    /// so a candidate they reject is absent from the page whatever `accept`
    /// would have said. Consulting it only on the admission path evaluates it
    /// strictly less often and never changes an answer. `seen` is still marked
    /// unconditionally, so which slots later passes re-offer is untouched.
    fn offer(&mut self, rank_key: &[u8], slot: u32, tier: Tier, base: f32) {
        // Marked before any test, so a slot dropped here still stops later
        // passes re-offering it — the bit means OFFERED, not accepted.
        self.seen[slot as usize >> 6] |= 1 << (slot & 63);
        let s = Scored {
            score: base * DEPTH_PEN[rank_key[slot as usize] as usize],
            eidx: slot,
            tier,
        };
        if self.heap.len() < self.cap {
            if !(self.accept)(slot) {
                return;
            }
            self.heap.push(Reverse(s));
            if self.heap.len() == self.cap {
                self.floor = self.heap.peek().expect("full").0.score;
            }
        } else if s.cmp(&self.heap.peek().expect("full").0) == Ordering::Greater {
            // Equal here means same score AND same slot, i.e. a duplicate
            // offer, which drain-dedup would collapse anyway.
            if !(self.accept)(slot) {
                return;
            }
            self.heap.pop();
            self.heap.push(Reverse(s));
            self.floor = self.heap.peek().expect("full").0.score;
        }
    }

    /// Drain to the final page: dedup by slot keeping the best offer, order by
    /// [`Scored::cmp`], truncate to `max_results`.
    fn drain(self, max_results: usize) -> Vec<Scored> {
        let mut all: Vec<Scored> = self.heap.into_iter().map(|r| r.0).collect();
        all.sort_unstable_by(|a, b| a.eidx.cmp(&b.eidx).then_with(|| b.cmp(a)));
        all.dedup_by_key(|s| s.eidx); // runs are best-first, so this keeps the max
        all.sort_unstable_by(|a, b| b.cmp(a));
        all.truncate(max_results);
        all
    }
}

/// Tiered search over one volume index (§3.4); called from
/// [`VolumeIndex::search`]. Returns up to `max_results` hits, best first.
pub(crate) fn search(
    ix: &VolumeIndex,
    query: &str,
    max_results: usize,
    is_cancelled: &dyn Fn() -> bool,
    accept: &dyn Fn(&str) -> bool,
) -> Vec<Hit> {
    let fq = fold(query);
    // `ix.is_empty()`, not `entries.is_empty()`: an index whose every entry has
    // been deleted still has a full entry table of tombstones.
    if fq.is_empty() || max_results == 0 || ix.is_empty() {
        return Vec::new();
    }
    // Queries arrive from the untrusted pipe (§3, §8.1) and U+0000 is the
    // folded arena's record delimiter, so a query carrying one is the single
    // input that could match across two records — the exact adjacency the
    // delimiters exist to forbid. Every other byte below 0x20 is likewise
    // absent from the arena by invariant I3, so it can only burn a full scan
    // to find nothing. Rejected, not stripped: quietly searching for
    // something the caller did not type is the worse of the two failures.
    if has_control_byte(&fq) {
        log::debug!("search: query with a reserved byte rejected");
        return Vec::new();
    }

    let rank_key = &ix.rank_key;
    // Slot -> name, so callers express the filter over names rather than over
    // the index's internal numbering.
    let accept_slot = |slot: u32| accept(ix.name_of_entry(&ix.entries[slot as usize]));
    let mut sel = Selector::new(max_results, ix.entries.len(), &accept_slot);
    let finder = memmem::Finder::new(fq.as_bytes());
    let qlen = fq.len();
    let mut processed = 0usize;
    let mut cancelled = is_cancelled();

    // Pass A: one scan of the folded name arena classifies the
    // exact / prefix / word-boundary / substring tiers per hit.
    //
    // Driven by hand rather than through `find_iter` for two reasons: the
    // cursor has to be visible so cancellation can be polled per MiB of arena
    // as well as per hit, and hits have to be groupable by record. Advancing by
    // `qlen` after every hit reproduces `find_iter`'s non-overlapping matches
    // exactly, so nothing about which hits are seen changes here.
    //
    // Skipped outright when the presence sets prove the arena cannot contain
    // the query — a few L1/L2 probes in place of a 24 MB scan, and half the
    // reason a query that matches nothing costs microseconds. These sets gate
    // THIS pass and no other: they describe the ARENA. Pass C never asks a
    // contiguity question at all, and Pass B asks one about a different column,
    // which is why it carries its own sets rather than borrowing these.
    if !cancelled && ix.arena_may_contain(fq.as_bytes()) {
        note_arena_scan();
        let arena = ix.folded_arena.as_bytes();
        // Hoisted out of the hit loop: three slices read in ascending order,
        // so the mapping streams instead of chasing a `Vec` header per hit.
        let (owner, recs, entries) = (&ix.owner[..], &ix.arena_recs[..], &ix.entries[..]);
        // Tier classification reads `arena[hit - 1]` and `arena[hit + qlen]`
        // unchecked; both are in bounds only because the arena opens and closes
        // with a fence. Step 9's in-place compaction truncates this arena, and
        // truncating one byte too far would turn that into a panic on the query
        // path — this makes it a failing test instead.
        debug_assert!(
            matches!(
                (arena.first(), arena.last()),
                (
                    Some(&crate::index::FOLDED_DELIM),
                    Some(&crate::index::FOLDED_DELIM)
                )
            ),
            "folded arena lost a fence"
        );
        // The record a run of consecutive hits belongs to, and the best tier
        // seen for it. All of a record's hits are adjacent in this ascending
        // scan, so this gives per-entry dedup — what the `best` map used to do
        // — for the cost of one compare.
        let mut pend: Option<(u32, Tier)> = None;
        let mut pos = 0usize;
        'scan: while pos + qlen <= arena.len() {
            if is_cancelled() {
                cancelled = true;
                break;
            }
            // The window carries the needle overlap ON TOP of the poll chunk,
            // so `pos` advances by SCAN_POLL_CHUNK + 1 bytes every iteration
            // whatever `qlen` is. Sizing it as just `pos + SCAN_POLL_CHUNK`
            // spins forever once `qlen > SCAN_POLL_CHUNK`: the resume point
            // below saturates to 0 and `pos` never moves. That is reachable
            // from the pipe — a 1 MiB frame (MAX_FRAME_C2S) of U+0130 folds to
            // ~1.5x its size, clearing the chunk — so it is a remote spin, not
            // a theoretical one. `SCAN_POLL_CHUNK.max(qlen)` does NOT fix it:
            // it degenerates to one byte of progress per full-window rescan.
            let win_end = (pos + SCAN_POLL_CHUNK + qlen).min(arena.len());
            let mut cursor = pos;
            while let Some(rel) = finder.find(&arena[cursor..win_end]) {
                let hit = cursor + rel;
                cursor = hit + qlen;
                processed += 1;
                if processed.is_multiple_of(CANCEL_STRIDE) && is_cancelled() {
                    cancelled = true;
                    break 'scan;
                }
                let tier = classify(arena[hit - 1], arena[hit + qlen]);
                // The reject that makes the tail cheap: two L1 bytes and one
                // float compare, no mapping, no column touch, no entry read.
                if sel.rejects(tier_upper_bound(tier)) {
                    continue;
                }
                let Some(slot) = slot_at(owner, recs, entries, hit, qlen) else {
                    continue;
                };
                match pend {
                    Some((prev, best)) if prev == slot => {
                        if tier_base(tier) > tier_base(best) {
                            pend = Some((prev, tier));
                        }
                    }
                    Some((prev, best)) => {
                        sel.offer(rank_key, prev, best, tier_base(best));
                        pend = Some((slot, tier));
                    }
                    None => pend = Some((slot, tier)),
                }
            }
            // No hit STARTS before `win_end - qlen + 1`, so resuming there
            // cannot re-find or skip anything; `cursor` covers the case where
            // the last hit ended past that point.
            pos = if win_end == arena.len() {
                arena.len()
            } else {
                cursor.max(win_end.saturating_sub(qlen - 1))
            };
        }
        if let Some((slot, tier)) = pend {
            sel.offer(rank_key, slot, tier, tier_base(tier));
        }
    }

    // Pass B: camel/initials — one scan of the fixed-stride initials column.
    //
    // Skipped outright for a query wider than a lane: such a needle can never
    // satisfy the containment test below, so scanning for it is pure cost. That
    // skip is what keeps the `exact` class off this array — at
    // `INITIALS_STRIDE` B/entry it is several times the packed arena it
    // replaces, and exact-match queries are exactly the long ones.
    //
    // Then the presence gate, which is to this column what `arena_may_contain`
    // is to the arena — and it has to be its OWN set: the arena's bigrams
    // describe whole folded names, not which letters are adjacent as segment
    // initials, so gating this pass on them would drop genuine camel-case
    // results. Without it this pass scanned the whole 8 MB column for every
    // query of 8 bytes or fewer — roughly two thirds of what a query matching
    // nothing cost, once Pass A had already early-outed (issue #8).
    //
    // Ordered cheapest-first, as the presence sets themselves are: `rejects` is
    // a length compare and a float compare that touch no memory, and it is the
    // more selective of the two in practice — it already takes this pass out
    // whenever a higher tier has filled the page. No point probing two tables
    // to answer a question the floor has settled.
    if !cancelled
        && qlen <= INITIALS_STRIDE
        && !sel.rejects(tier_upper_bound(Tier::Initials))
        && ix.initials_may_contain(fq.as_bytes())
    {
        note_initials_scan();
        let lanes = &ix.initials[..];
        debug_assert_eq!(lanes.len(), ix.entries.len() * INITIALS_STRIDE);
        let mut pend: Option<u32> = None;
        let mut cursor = 0usize;
        while let Some(rel) = finder.find(&lanes[cursor..]) {
            let hit = cursor + rel;
            processed += 1;
            if processed.is_multiple_of(CANCEL_STRIDE) && is_cancelled() {
                cancelled = true;
                break;
            }
            // The floor only rises, so once the tier is out it stays out.
            if sel.rejects(tier_upper_bound(Tier::Initials)) {
                break;
            }
            if hit % INITIALS_STRIDE + qlen > INITIALS_STRIDE {
                // Straddles two lanes, so it spells no name. Resume ONE byte
                // on, not `qlen` on: a full lane has no NUL padding to fence
                // it, so `…a | aab…` produces a straddling `aa` at the lane
                // boundary whose rejection would otherwise skip the genuine
                // `aa` starting the next lane — the same shape as the arena
                // straddle SPEC DECISION 8 retired. Rejected hits are rare
                // (they need a lane packed to all 8 bytes), so the byte-at-a-
                // time resume costs nothing measurable.
                cursor = hit + 1;
                continue;
            }
            cursor = hit + qlen;
            // The whole point of the fixed stride: no table, no search.
            let slot = (hit / INITIALS_STRIDE) as u32;
            match pend {
                Some(prev) if prev == slot => {}
                Some(prev) => {
                    sel.offer(rank_key, prev, Tier::Initials, tier_base(Tier::Initials));
                    pend = Some(slot);
                }
                None => pend = Some(slot),
            }
        }
        if let Some(slot) = pend {
            sel.offer(rank_key, slot, Tier::Initials, tier_base(Tier::Initials));
        }
    }

    // Pass C: fuzzy subsequence over character-class superset candidates.
    // Queries shorter than 3 bytes skip the tier entirely (§3.4), and so does
    // a page already filled above the fuzzy ceiling.
    //
    // Deliberately NOT gated on the presence sets, unlike Pass A. Fuzzy matches
    // a SUBSEQUENCE, so a query whose trigrams appear nowhere in the arena can
    // still have genuine matches — `abcd` against `a_b_c_d` is the canonical
    // one, and it is precisely what the old trigram-intersection prefilter
    // dropped (behavior change 1: this pass now returns strictly more).
    if !cancelled && qlen >= 3 && !sel.rejects(tier_upper_bound(Tier::Fuzzy)) {
        // Collected ONCE per search, not once per candidate. Stack-resident for
        // every query a human types; the `Vec` is the correctness fallback for
        // a pipe-sized one, and is still a single allocation for the search
        // rather than two per verified candidate.
        let mut qbuf = ['\0'; FUZZY_QBUF];
        let mut qvec: Vec<char> = Vec::new();
        let qn = fq.chars().count();
        let qchars: &[char] = if qn <= FUZZY_QBUF {
            for (slot, c) in qbuf.iter_mut().zip(fq.chars()) {
                *slot = c;
            }
            &qbuf[..qn]
        } else {
            qvec.extend(fq.chars());
            &qvec
        };

        // Depth ceiling implied by the running floor. A fuzzy candidate's base
        // is at most `tier_upper_bound(Fuzzy)` whatever its density, so at
        // cached depth `d` it tops out at `0.5 · DEPTH_PEN[d]` — the
        // unpenalized entry of the table, which bounds the hidden/system case
        // too since that only multiplies by 0.85. Derived by walking the same
        // table the scorer reads rather than from the closed form, so the
        // comparison is bit-identical to the one `offer` would make. STRICT, as
        // everywhere: on equality the candidate still has the `eidx` tie-break.
        // `sel.floor` is `NEG_INFINITY` until the heap fills and never falls
        // afterwards, so it needs no `full()` guard: before the page is full
        // nothing compares below it and `d_max` stays wide open.
        let mut d_max = RANK_DEPTH_MAX;
        while d_max > 0 && tier_upper_bound(Tier::Fuzzy) * DEPTH_PEN[d_max as usize] < sel.floor {
            d_max -= 1;
        }

        let (bsi, wps) = (&ix.charclass_bsi[..], ix.bsi_words);
        let words = ix.entries.len().div_ceil(64).min(wps);
        let qmask = class_mask(&fq);
        // Whether a survivor is worth verifying at all, before it is counted or
        // scored: already claimed by a strictly better tier, or too deep to
        // reach the page.
        let admits = |sel: &Selector, slot: u32| {
            !sel.is_seen(slot) && rank_key[slot as usize] & RANK_DEPTH_MAX <= d_max
        };

        // Two passes over the slices rather than one pass into a candidate
        // vector: the AND is ~1 MB of streaming reads at 1M, where the vector
        // would be up to 4 MB of transient allocation on a query whose classes
        // are common. The first pass only counts, by depth.
        let mut by_depth = [0u32; RANK_DEPTH_MAX as usize + 1];
        let mut survivors = 0usize;
        for_each_class_superset(bsi, wps, words, qmask, |slot| {
            if admits(&sel, slot) {
                by_depth[(rank_key[slot as usize] & RANK_DEPTH_MAX) as usize] += 1;
                survivors += 1;
            }
            true
        });

        // The FUZZY_CAP drain, SHALLOWEST FIRST (behavior change 9). Fuzzy base
        // is ≤ 0.5 regardless of density, so `score = base · DEPTH_PEN[depth]`
        // means a shallow candidate dominates a deep one whatever they contain:
        // the candidates the cap drops are exactly the ones least able to reach
        // the page. What it replaces was "the first 20,000 in arena order",
        // which is an arbitrary subset. `cut` is the depth the budget runs out
        // at and `quota` is how many of that depth still fit.
        let mut cut = RANK_DEPTH_MAX as usize;
        let mut quota = FUZZY_CAP;
        if survivors > FUZZY_CAP {
            let mut acc = 0usize;
            for (d, &n) in by_depth.iter().enumerate() {
                if acc + n as usize >= FUZZY_CAP {
                    cut = d;
                    quota = FUZZY_CAP - acc;
                    break;
                }
                acc += n as usize;
            }
        }

        let mut scored = 0usize;
        for_each_class_superset(bsi, wps, words, qmask, |slot| {
            if !admits(&sel, slot) {
                return true;
            }
            let d = (rank_key[slot as usize] & RANK_DEPTH_MAX) as usize;
            if d > cut {
                return true;
            }
            if d == cut {
                if quota == 0 {
                    return true;
                }
                quota -= 1;
            }
            scored += 1;
            if scored.is_multiple_of(CANCEL_STRIDE) && is_cancelled() {
                cancelled = true;
                return false;
            }
            let e = &ix.entries[slot as usize];
            if let Some(density) = fuzzy_density(ix.folded_of_entry(e), qchars) {
                sel.offer(rank_key, slot, Tier::Fuzzy, 0.3 + 0.2 * density);
            }
            true
        });
    }
    if cancelled {
        log::trace!("search cancelled early; returning partial results");
    }

    let top = sel.drain(max_results);

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
    use std::collections::{btree_map, BTreeMap};

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

    /// [`fuzzy_density`] with the query collected for you. The real caller
    /// collects once per SEARCH into a stack buffer, which is the whole reason
    /// the function takes `&[char]`; tests want to write the query as a
    /// literal.
    fn density(name: &str, q: &str) -> Option<f32> {
        let qc: Vec<char> = q.chars().collect();
        fuzzy_density(name, &qc)
    }

    /// Pass A arena scans started since the last call, and reset.
    fn arena_scans_since() -> usize {
        ARENA_SCANS.with(|c| c.replace(0))
    }

    /// Pass B initials-column scans started since the last call, and reset.
    fn initials_scans_since() -> usize {
        INITIALS_SCANS.with(|c| c.replace(0))
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

    /// SPEC DECISION 1 / behavior change 1: a genuinely gapped subsequence now
    /// reaches the fuzzy tier.
    ///
    /// **This fails on the prefilter it replaces.** Trigram intersection asks
    /// for postings on `abc` and `bcd`; `abxcxd` carries neither, so the
    /// lookup missed and the tier returned nothing — a false NEGATIVE in a
    /// prefilter, and the exact reason bench.rs has to SYNTHESIZE
    /// `plant_fuzzy_name` corpus members before the fuzzy class measures the
    /// tier rather than the early-out. A character-class mask cannot have one:
    /// every byte of a subsequence is a byte of the name, so the name's mask is
    /// a superset by construction.
    ///
    /// The name is deliberately unsegmented. `a_b_c_d` — the shape the design
    /// document quotes — would answer this query from the INITIALS tier at 0.7
    /// whatever the fuzzy prefilter did, and would prove nothing.
    #[test]
    fn gapped_subsequence_reaches_the_fuzzy_tier() {
        let mut v = ix();
        v.add(1, 999, "abxcxd", 0);
        // One segment, so its lane is "a" — the initials tier cannot answer.
        assert_eq!(&initials_lane("abxcxd")[..1], b"a");
        let hits = v.search("abcd", 10, &no_cancel);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].frn, 1);
        // 4 chars matched over a span of 6.
        assert!(approx(hits[0].score, 0.3 + 0.2 * (4.0 / 6.0)));
        assert_eq!(hits[0].match_ranges, vec![(0, 2), (3, 4), (5, 6)]);
    }

    /// A presence-set miss must answer without touching the arena at all.
    ///
    /// The corpus is built so ONLY `tri_present` can decide: every byte and
    /// every bigram of `abc` is in the arena, split across two names, and the
    /// trigram is in neither. That is the case `byte_hist`-style filters
    /// provably cannot catch, and it is why the set is exact rather than a
    /// histogram.
    #[test]
    fn presence_miss_returns_empty_without_scanning_the_arena() {
        let mut v = ix();
        v.add(1, 999, "qab", 0);
        v.add(2, 999, "bcq", 0);
        for b in *b"abc" {
            assert!(v.arena_may_contain(&[b]), "unigram {b} should be present");
        }
        assert!(v.arena_may_contain(b"ab") && v.arena_may_contain(b"bc"));
        assert!(
            !v.arena_may_contain(b"abc"),
            "trigram must be the only miss"
        );

        arena_scans_since();
        assert!(v.search("abc", 10, &no_cancel).is_empty());
        assert_eq!(arena_scans_since(), 0, "the arena must not be scanned");

        // Control, so the counter is proved to count: a query the arena does
        // hold scans exactly once.
        assert!(!v.search("qab", 10, &no_cancel).is_empty());
        assert_eq!(arena_scans_since(), 1);
    }

    /// …and the same miss must NOT take the fuzzy tier down with it.
    ///
    /// The presence sets answer a question about CONTIGUITY. Fuzzy matches a
    /// subsequence, so gating Pass C on them would delete exactly the results
    /// behavior change 1 exists to add — silently, and only for queries whose
    /// trigrams happen to be absent, which is the common case for a gapped
    /// query. Same corpus as above plus one name that `abc` reaches by gap.
    #[test]
    fn presence_miss_does_not_suppress_a_fuzzy_hit() {
        let mut v = ix();
        v.add(1, 999, "qab", 0);
        v.add(2, 999, "bcq", 0);
        v.add(3, 999, "azbzc", 0);
        assert!(!v.arena_may_contain(b"abc"));

        arena_scans_since();
        let hits = v.search("abc", 10, &no_cancel);
        assert_eq!(arena_scans_since(), 0, "Pass A must still be skipped");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].frn, 3);
        // 3 chars matched over a span of 5.
        assert!(approx(hits[0].score, 0.3 + 0.2 * (3.0 / 5.0)));
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

    /// A needle longer than [`SCAN_POLL_CHUNK`] must still terminate. Pass A
    /// resumes each window at `win_end - (qlen - 1)`; if the window is only
    /// `SCAN_POLL_CHUNK` wide that saturates to the window start and the scan
    /// spins forever, escapable only by cancellation. Reachable from the pipe:
    /// a legal 1 MiB frame folds larger than it arrives (U+0130 → 3 bytes), so
    /// this is a remote spin. `no_cancel` is deliberate — a cancelling closure
    /// would mask the hang this pins down.
    #[test]
    fn needle_longer_than_the_poll_chunk_terminates() {
        let mut v = ix();
        for i in 0..8000u64 {
            v.add(i + 1, 999, &format!("file{i}.txt"), 0);
        }
        assert!(
            v.folded_arena_len() < SCAN_POLL_CHUNK,
            "the arena must be smaller than the needle for this to bite"
        );
        let needle = "b".repeat(SCAN_POLL_CHUNK + 1);
        assert!(v.search(&needle, 8, &no_cancel).is_empty());
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

    /// SPEC DECISION 8 — a straddling hit must not HIDE a real one.
    ///
    /// `memmem::find_iter` yields NON-overlapping matches. Before the arena
    /// was NUL-delimited it read `"xaaab"`, the only hit for `"aa"` was the
    /// cross-record one at offset 1, the bounds check dropped it, and the
    /// finder resumed at offset 3 — past the genuine prefix match on `"aab"`
    /// at offset 2. The entry was silently unreachable. One delimiter byte per
    /// record makes the straddle impossible to form, so no hit is ever
    /// rejected and no resume point can skip a match.
    #[test]
    fn adjacent_records_cannot_hide_a_real_match() {
        let mut v = ix();
        v.add(1, 999, "xa", 0);
        v.add(2, 999, "aab", 0);
        let hits = v.search("aa", 10, &no_cancel);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].frn, 2);
        assert!(approx(hits[0].score, 0.9)); // a real prefix hit, not a ghost
        assert_eq!(hits[0].match_ranges, vec![(0, 2)]);
    }

    /// Queries arrive off the untrusted pipe (§3, §8.1) and U+0000 is the
    /// folded arena's record delimiter, so a control byte in the folded query
    /// is rejected at the top of `search` (invariant I3). Without this, an
    /// attacker-shaped query could address a delimiter and match across two
    /// records — the exact adjacency the delimiters exist to forbid.
    #[test]
    fn control_byte_query_is_rejected_and_never_spans_a_delimiter() {
        let mut v = ix();
        v.add(1, 999, "abc", 0);
        v.add(2, 999, "def", 0);
        // "c\0d" is literally present in the arena as `…abc\0def…`.
        assert!(v.search("c\u{0}d", 10, &no_cancel).is_empty());
        assert!(v.search("\u{0}", 10, &no_cancel).is_empty());
        assert!(v.search("abc\u{0}", 10, &no_cancel).is_empty());
        assert!(v.search("\u{0}abc", 10, &no_cancel).is_empty());
        // Same rule for every other control byte, none of which can occur in
        // the arena either (I3 is enforced at `intern` too).
        assert!(v.search("a\tb", 10, &no_cancel).is_empty());
        assert!(v.search("ab\u{1f}", 10, &no_cancel).is_empty());
        // The guard rejects control bytes, not queries.
        assert_eq!(v.search("abc", 10, &no_cancel).len(), 1);
    }

    /// BEHAVIOR CHANGE 2, asserted as a decision rather than discovered as a
    /// surprise: the initials tier lives in a fixed [`INITIALS_STRIDE`]-byte
    /// lane, so a name with more initials than fit LOSES the trailing ones, and
    /// a query wider than a lane cannot reach the tier at all.
    ///
    /// Nine segments, nine initials, eight bytes of lane. The name is built so
    /// none of the queries below can be answered by any other tier — its folded
    /// form is `"al be ce de ee fe ge he ie"`, which contains none of `gh`,
    /// `hi`, `abcdefgh` or `abcdefghi` — so every assertion is about Pass B
    /// alone.
    ///
    /// What is NOT observable, and deliberately so: `search`'s
    /// `qlen ≤ INITIALS_STRIDE` skip. A query wider than a lane could never
    /// satisfy the containment test either, so the skip is a cost saving with
    /// no result attached — which is exactly why it is safe to make.
    #[test]
    fn initials_past_the_eight_byte_lane_are_truncated() {
        let mut v = ix();
        v.add(1, 999, "Al Be Ce De Ee Fe Ge He Ie", 0);
        assert_eq!(segment_spans("Al Be Ce De Ee Fe Ge He Ie").len(), 9);

        // The lane holds the first eight initials, and only those.
        let slot = v.frn_map[&1] as usize;
        assert_eq!(
            &v.initials[slot * INITIALS_STRIDE..(slot + 1) * INITIALS_STRIDE],
            b"abcdefgh"
        );

        // Eight fit, and the tier answers for them.
        let hits = v.search("abcdefgh", 10, &no_cancel);
        assert_eq!(hits.len(), 1);
        assert!(approx(hits[0].score, 0.7));
        assert_eq!(v.search("gh", 10, &no_cancel).len(), 1);

        // The ninth is gone: no query that needs it can reach the 0.7 tier.
        // The nine-initial query does still come back — as a 0.372 FUZZY row,
        // because those nine letters are a genuine gapped subsequence of the
        // name and the class-mask prefilter no longer loses it (behavior
        // change 1; trigram intersection returned nothing here). That is a
        // different tier at half the score, so the initials truncation being
        // signed off is unchanged.
        let hits = v.search("abcdefghi", 10, &no_cancel);
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].score < tier_base(Tier::Initials),
            "must not reach the initials tier: {}",
            hits[0].score
        );
        assert!(approx(hits[0].score, 0.3 + 0.2 * (9.0 / 25.0)));
        // Two bytes: below the fuzzy floor (§3.4), so nothing at all.
        assert!(v.search("hi", 10, &no_cancel).is_empty());
    }

    /// A rejected lane-straddling hit must not HIDE a real one — the initials
    /// column's version of the arena straddle SPEC DECISION 8 retired.
    ///
    /// A lane packed to all eight bytes has no NUL padding to fence it, so a
    /// needle can match across two lanes. That hit spells no name and is
    /// rejected, but resuming `qlen` bytes on (what `find_iter` would do) skips
    /// the byte after the boundary — and that byte is where the next lane, and
    /// a genuine match, starts. Pass B therefore resumes ONE byte on after a
    /// rejection.
    ///
    /// The arrangement is forced, not chosen: for a 2-byte needle the straddle
    /// sits at lane byte 7, so the hidden match starts at the next lane's byte
    /// 0, which requires `q[0] == q[1]`. Hence `"aa"`, a full lane ending in
    /// `a`, and a next lane beginning `aa`.
    #[test]
    fn a_straddling_initials_hit_cannot_hide_a_real_one() {
        let mut v = ix();
        // Slot 0: eight segments → a full lane, last initial 'a'.
        v.add(1, 999, "Zz Zz Zz Zz Zz Zz Zz Ab", 0);
        // Slot 1: initials "aab", so "aa" starts its lane.
        v.add(2, 999, "Ax Ay Bz", 0);
        let (s0, s1) = (v.frn_map[&1] as usize, v.frn_map[&2] as usize);
        assert_eq!(
            &v.initials[s0 * INITIALS_STRIDE..(s0 + 1) * INITIALS_STRIDE],
            b"zzzzzzza",
            "slot 0's lane must be FULL for the straddle to form"
        );
        assert_eq!(
            &v.initials[s1 * INITIALS_STRIDE..(s1 + 1) * INITIALS_STRIDE],
            b"aab\0\0\0\0\0"
        );
        assert_eq!(s1, s0 + 1, "the lanes must be adjacent");
        // Neither folded name contains "aa", so only the initials tier can
        // answer — and it must, for the slot whose lane genuinely starts "aa".
        let hits = v.search("aa", 10, &no_cancel);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].frn, 2);
        assert!(approx(hits[0].score, 0.7));
    }

    /// A tombstoned slot's lane is zeroed, so the tier stops answering for it
    /// immediately — Pass B scans the column with no liveness check, and NUL
    /// can never appear in a folded query (invariant I3).
    #[test]
    fn a_deleted_entry_leaves_the_initials_tier() {
        let mut v = ix();
        v.add(1, 999, "Foo Bar.txt", 0);
        v.add(2, 999, "Foo Qux.txt", 0);
        assert_eq!(v.search("fb", 10, &no_cancel).len(), 1);
        let slot = v.frn_map[&1] as usize;
        v.apply(crate::UsnEvent::Delete { frn: 1 });
        assert_eq!(
            &v.initials[slot * INITIALS_STRIDE..(slot + 1) * INITIALS_STRIDE],
            &[0u8; INITIALS_STRIDE]
        );
        assert!(v.search("fb", 10, &no_cancel).is_empty());
        // The survivor is untouched, and a recycled slot answers for its new
        // occupant only.
        assert_eq!(v.search("fq", 10, &no_cancel).len(), 1);
        v.add(3, 999, "Ping Pong.txt", 0);
        assert_eq!(v.frn_map[&3] as usize, slot, "the slot must be recycled");
        assert!(v.search("fb", 10, &no_cancel).is_empty());
        assert_eq!(v.search("pp", 10, &no_cancel)[0].frn, 3);
    }

    /// A rename rewrites the lane in place, so the old initials stop matching
    /// and the new ones start — no rebuild, and the same slot throughout.
    #[test]
    fn a_rename_rewrites_the_initials_lane_in_place() {
        let mut v = ix();
        v.add(1, 999, "Foo Bar.txt", 0);
        let slot = v.frn_map[&1];
        v.apply(crate::UsnEvent::Rename {
            frn: 1,
            new_parent_frn: 999,
            new_name: "Quux Zed.txt".into(),
        });
        assert_eq!(v.frn_map[&1], slot);
        assert!(v.search("fb", 10, &no_cancel).is_empty());
        let hits = v.search("qz", 10, &no_cancel);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].frn, 1);
        assert!(approx(hits[0].score, 0.7));
    }

    /// A query no lane carries must answer without scanning the column.
    ///
    /// This is Pass B's half of the presence machinery, and it needs its OWN
    /// set: `qz` here is a perfectly ordinary pair of arena bigrams — `quux
    /// zed` contains both letters and the arena's `bi_present` says nothing
    /// about whether they are ever adjacent as segment INITIALS. Gating this
    /// pass on the arena's sets would have dropped the `fb` hit below.
    #[test]
    fn an_initials_presence_miss_skips_the_column_scan() {
        let mut v = ix();
        v.add(1, 999, "Foo Bar.txt", 0);
        v.add(2, 999, "quux zed.txt", 0);

        // Lanes are `fbt` and `qzt` — the extension is a segment too. So both
        // bytes of `fz` are in the column and `fb` is a genuine lane bigram:
        // only the initials BIGRAM set can decide this query.
        assert_eq!(&initials_lane("Foo Bar.txt")[..3], b"fbt");
        assert_eq!(&initials_lane("quux zed.txt")[..3], b"qzt");
        assert!(v.initials_may_contain(b"f") && v.initials_may_contain(b"z"));
        assert!(v.initials_may_contain(b"fb"), "fb is a real lane bigram");
        assert!(
            !v.initials_may_contain(b"fz"),
            "`f` and `z` are never adjacent initials"
        );

        initials_scans_since();
        assert!(v.search("fz", 10, &no_cancel).is_empty());
        assert_eq!(
            initials_scans_since(),
            0,
            "the initials column must not be scanned"
        );

        // Control, so the counter is proved to count: a query the column does
        // hold scans exactly once.
        assert_eq!(v.search("fb", 10, &no_cancel)[0].frn, 1);
        assert_eq!(initials_scans_since(), 1);

        // And the UNIGRAM half carries the gate on its own for a one-byte
        // query, which has no bigram window at all. Without this the unigram
        // set is redundant — a set bigram bit already implies both its bytes
        // were noted — and deleting it would pass every other test here while
        // making every one-byte miss re-scan the whole column.
        assert!(!v.initials_may_contain(b"w"), "no lane carries `w`");
        initials_scans_since();
        assert!(v.search("w", 10, &no_cancel).is_empty());
        assert_eq!(
            initials_scans_since(),
            0,
            "a one-byte miss must skip the column too"
        );
    }

    /// …and the same miss must NOT take the other tiers down with it.
    ///
    /// Two presence families over two different columns, and neither may leak
    /// into the other's pass. `ackup` is absent from every lane — no name has
    /// five segments starting a,c,k,u,p — but it is a plain substring of
    /// `backup`, which Pass A must still return.
    #[test]
    fn an_initials_miss_does_not_suppress_a_substring_hit() {
        let mut v = ix();
        v.add(1, 999, "backup.txt", 0);

        assert!(!v.initials_may_contain(b"ackup"));

        initials_scans_since();
        let hits = v.search("ackup", 10, &no_cancel);
        assert_eq!(initials_scans_since(), 0, "Pass B must still be skipped");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].frn, 1);
        // Contiguous, not at a word boundary: the substring tier.
        assert!(approx(hits[0].score, 0.55));
    }

    /// The sets are built PER LANE, so a bigram that exists only across a lane
    /// boundary must miss — and missing must be result-neutral, because the
    /// containment test rejects such a hit anyway.
    #[test]
    fn an_initials_bigram_never_spans_two_lanes() {
        let mut v = ix();
        // Lanes are `…a` and `b…`, so the column bytes read `…a` `b…` and a
        // naive scan of the whole array would find `ab` straddling them.
        v.add(1, 999, "xylophone alpha", 0);
        v.add(2, 999, "beta gamma", 0);
        assert_eq!(&initials_lane("xylophone alpha")[..2], b"xa");
        assert_eq!(&initials_lane("beta gamma")[..2], b"bg");

        assert!(
            !v.initials_may_contain(b"ab"),
            "`ab` exists only across the lane boundary"
        );
        // And rejecting it costs nothing, because Pass B would have thrown the
        // straddling hit away: neither before nor after the gate is there a
        // result here.
        assert!(v.search("ab", 10, &no_cancel).is_empty());
    }

    /// The sets cover the WHOLE fixed-width lane, so a query matching a full
    /// eight-byte lane or its interior is admitted. A construction that read
    /// only a lane's leading bytes fails here.
    #[test]
    fn the_initials_sets_cover_the_whole_lane() {
        let mut v = ix();
        v.add(
            1,
            999,
            "Alpha Bravo Charlie Delta Echo Foxtrot Golf Hotel",
            0,
        );
        assert_eq!(
            &initials_lane("Alpha Bravo Charlie Delta Echo Foxtrot Golf Hotel"),
            b"abcdefgh"
        );

        // Full lane, no padding at all.
        assert!(v.initials_may_contain(b"abcdefgh"));
        // Lane-interior, which a leading-bytes-only set would miss.
        assert!(v.initials_may_contain(b"gh"));
        assert!(v.initials_may_contain(b"de"));
        assert_eq!(v.search("gh", 10, &no_cancel)[0].frn, 1);
    }

    /// Deleting from a column whose every lane is packed full is still
    /// consistent.
    ///
    /// The one lane write that does NOT record its bytes is
    /// `unregister_slot`'s NUL fill, and `compact` refills from live lanes
    /// only — so if no live lane ever carried padding, `FOLDED_DELIM` is
    /// absent from the sets and a delete leaves an all-NUL lane nothing
    /// recorded. Harmless, because invariant I3 keeps bytes below 0x20 out of
    /// every folded query, so no gate decision can read those bits; the point
    /// of this test is that the invariant checker agrees, rather than
    /// reporting a violation the first time a corpus has full lanes and a
    /// delete.
    #[test]
    fn deleting_from_a_fully_packed_column_keeps_the_invariants() {
        let mut v = ix();
        v.add(1, 999, "Al Bo Ca Da Ez Fo Gu Hi", 0);
        v.add(2, 999, "Ja Iv Hi Gu Fo Ez Da Ca", 0);
        assert_eq!(&initials_lane("Al Bo Ca Da Ez Fo Gu Hi"), b"abcdefgh");
        assert_eq!(&initials_lane("Ja Iv Hi Gu Fo Ez Da Ca"), b"jihgfedc");
        assert_invariants(&v, "packed column");

        v.apply(crate::UsnEvent::Delete { frn: 1 });
        assert_invariants(&v, "after deleting a packed lane");
        // The survivor is still reachable, and the deleted one is gone.
        assert_eq!(v.search("jihg", 10, &no_cancel)[0].frn, 2);
        assert!(v.search("abcd", 10, &no_cancel).is_empty());
    }

    /// The lane packs FOLDED initials and never a partial character: an initial
    /// whose folded form does not fit is dropped whole, and the walk stops
    /// there rather than pulling a later, narrower initial into its place.
    #[test]
    fn initials_lane_is_folded_nul_padded_and_never_half_a_char() {
        assert_eq!(&initials_lane("FooBar.txt"), b"fbt\0\0\0\0\0");
        assert_eq!(&initials_lane("my-file.txt"), b"mft\0\0\0\0\0");
        assert_eq!(&initials_lane("..."), &[0u8; INITIALS_STRIDE]);
        // Exactly full, no padding.
        assert_eq!(&initials_lane("a b c d e f g h"), b"abcdefgh");
        // Two 3-byte CJK initials fit; a third would need 9 bytes of 8, so it
        // is dropped WHOLE — never truncated to the two bytes that are free,
        // which would leave the lane spelling no character at all.
        let mut cjk = [0u8; INITIALS_STRIDE];
        cjk[.."汉语".len()].copy_from_slice("汉语".as_bytes());
        assert_eq!(initials_lane("汉 语 手 册"), cjk);
        // …and the walk STOPS there: the one-byte 'z' that follows must not
        // slide into the free bytes, which would spell an order of initials the
        // name has not got and answer a query it should not.
        assert_eq!(initials_lane("汉 语 手 z"), cjk);
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
        assert!(approx(density("abcd", "abcd").unwrap(), 1.0));
        assert!(approx(density("abcx_bcd", "abcd").unwrap(), 0.5));
        assert_eq!(density("abc", "abd"), None);
        assert_eq!(density("ab", "abc"), None);
        assert_eq!(density("abc", ""), None);
        // The verifier is allocation-free over multi-byte chars too: the
        // backward pass counts CHARS while iterating bytes, so a name whose
        // chars are 3 bytes wide must still score by char span, not byte span.
        assert!(approx(density("汉x语", "汉语").unwrap(), 2.0 / 3.0));
    }

    // -----------------------------------------------------------------
    // The differential test.
    //
    // `search` now prunes: it skips whole passes and rejects individual hits
    // against a running floor, and it keeps only 2·max_results offers instead
    // of every candidate. Both rules must be RESULT-PRESERVING, and no other
    // test in this file can show that they are — `max_results_caps_and_orders`
    // only asserts descending order and has no tie at the K boundary. The
    // failure mode is silent: `Scored::cmp` breaks an equal score by LOWER
    // eidx, so a `≥` where the design says `>` discards tie-break winners and
    // every existing assertion still passes.
    //
    // So: an exhaustive reference scorer with no heap, no capacity and no
    // floor, run against the real selector over hundreds of randomized
    // corpora deliberately built so that equal scores land at the K boundary
    // — a six-word alphabet (so names repeat verbatim) parented into a handful
    // of directories (so repeated names share a depth, hence an exactly equal
    // score) and read back at K = 1..8, where the boundary is a tie almost
    // every time.
    // -----------------------------------------------------------------

    /// xorshift64*, so a failure names a seed rather than a mood.
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed | 1)
        }
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Deliberately tiny, so names collide verbatim and scores tie exactly.
    /// `AaBbCc` and `Ärger` are here for the initials column specifically:
    /// without them every generated name is 1-3 single-segment words, so lanes
    /// never exceed 3 of their 8 bytes and every initial is one byte. The
    /// camel word contributes three initials per segment, so three of them
    /// overflow the lane and exercise `initials_lane`'s drop-whole truncation;
    /// the accented one gives a two-byte initial. Both are cases the presence
    /// sets' invariant would otherwise never see. Neither introduces a `d`,
    /// which the `abcd` note below depends on.
    const WORDS: [&str; 8] = ["aa", "ab", "ba", "bb", "ac", "ca", "AaBbCc", "Ärger"];
    const SEPS: [char; 4] = ['-', '_', '.', ' '];

    /// Queries across every tier: whole words (exact/prefix), separator-led
    /// (word boundary), initial pairs and triples (initials), interior slices
    /// (substring), gapped and 3+ byte forms (fuzzy), and misses.
    ///
    /// `FUZZY_PROBE` is the one query the random alphabet cannot produce, and
    /// it is what puts the fuzzy tier's own bound under test — see
    /// [`plant_fuzzy_band`].
    const FUZZY_PROBE: &str = "abcd";
    const QUERIES: [&str; 31] = [
        "a",
        "b",
        "c",
        "aa",
        "ab",
        "ba",
        "bb",
        "ac",
        "ca",
        "abc",
        "aab",
        "aba",
        "a-a",
        "a_b",
        "a.b",
        "a b",
        "-a",
        "aa-",
        "aaa",
        "abab",
        "aabb",
        "bac",
        "aa-b",
        "b.c",
        "cab",
        "aacb",
        "aaaa",
        "cc",
        "zz",
        "abcabc",
        FUZZY_PROBE,
    ];

    fn random_name(rng: &mut Rng) -> String {
        let mut s = String::new();
        for i in 0..1 + rng.below(3) {
            if i > 0 {
                s.push(SEPS[rng.below(SEPS.len())]);
            }
            let w = WORDS[rng.below(WORDS.len())];
            // Camel-case some segments so the initials tier sees more than one
            // initial per separator-delimited run.
            if rng.below(2) == 0 {
                s.push_str(&w.to_uppercase());
            } else {
                s.push_str(w);
            }
        }
        s
    }

    /// FRN nothing in a generated corpus mints, so naming it as a parent puts
    /// an entry at depth 0.
    const NO_PARENT: u64 = 999_999;

    /// Plant the one arrangement that puts `tier_upper_bound(Fuzzy)` itself
    /// under test, because the random alphabet cannot produce it.
    ///
    /// The bound only ever decides anything when the heap is already full and
    /// the floor sits BELOW the fuzzy ceiling — i.e. when a fuzzy candidate can
    /// still win the page. So: a deep, hidden family that matches
    /// [`FUZZY_PROBE`] as a mere substring (`0.55 × 0.85 / 1.24 ≈ 0.377`, and
    /// enough of them to fill any small page), plus shallow unattributed names
    /// that the query only reaches as a GAPPED subsequence
    /// (`0.3 + 0.2 × 4/6 ≈ 0.433`). The fuzzy row outranks the substring rows,
    /// so skipping Pass C changes the answer.
    ///
    /// This is what a bound of `tier_base(Fuzzy)` = 0.3 would break: 0.3 is
    /// below the 0.377 floor, so the whole pass would be skipped and the 0.433
    /// row would silently vanish. `abcd` is unreachable from `WORDS` (no `d`),
    /// and `abcbcd` carries both of the query's trigrams without carrying the
    /// query itself, so nothing else in the corpus interferes.
    fn plant_fuzzy_band(v: &mut VolumeIndex) {
        const DEEP: u64 = 12;
        for d in 0..=DEEP {
            let parent = if d == 0 { NO_PARENT } else { 20 + d - 1 };
            v.add(20 + d, parent, &format!("deep{d}"), crate::flags::DIR);
        }
        for i in 0..6u64 {
            v.add(40 + i, 20 + DEEP, "xabcd", crate::flags::HIDDEN);
        }
        for i in 0..3u64 {
            v.add(60 + i, NO_PARENT, "abcbcd", 0);
        }
    }

    /// A corpus shaped to make the K boundary a TIE as often as possible, which
    /// is the only condition under which a non-strict pruning rule misbehaves.
    ///
    /// Three properties do that work, and all three are load-bearing:
    /// - most entries sit at depth 0 and carry no attributes, so their score is
    ///   *exactly* `tier_base` — the only way `tier_upper_bound(t)` can equal
    ///   the running floor rather than merely approach it;
    /// - names come from a six-word alphabet, so they repeat verbatim and
    ///   identical names at identical depths score bit-identically;
    /// - the corpus is churned, so slots come off the freelist and arena order
    ///   stops agreeing with slot order — a tie-break WINNER (lower eidx) then
    ///   arrives after the heap is already full, which is exactly the offer a
    ///   `≥` rule would throw away.
    fn random_corpus(seed: u64, files: usize) -> VolumeIndex {
        let mut rng = Rng::new(seed);
        let mut v = ix();
        // A few directories at assorted depths, so the depth penalty still
        // spreads some scores apart.
        let dirs: Vec<u64> = (0..4u64).map(|i| 10 + i).collect();
        for i in 0..dirs.len() {
            let parent = if i == 0 {
                NO_PARENT
            } else {
                dirs[rng.below(i)]
            };
            let name = random_name(&mut rng);
            v.add(dirs[i], parent, &name, crate::flags::DIR);
        }
        let place = |rng: &mut Rng, v: &mut VolumeIndex, frn: u64| {
            // Two thirds at depth 0 with no attributes: score == tier_base.
            let parent = match rng.below(3) {
                0 => dirs[rng.below(dirs.len())],
                _ => NO_PARENT,
            };
            let attrs = match rng.below(8) {
                0 => crate::flags::HIDDEN,
                1 => crate::flags::SYSTEM,
                _ => 0,
            };
            let name = random_name(rng);
            v.add(frn, parent, &name, attrs);
        };
        for k in 0..files {
            place(&mut rng, &mut v, 1000 + k as u64);
        }
        for _ in 0..files / 4 {
            v.apply(crate::UsnEvent::Delete {
                frn: 1000 + rng.below(files) as u64,
            });
        }
        for k in 0..files / 4 {
            place(&mut rng, &mut v, 5000 + k as u64);
        }
        plant_fuzzy_band(&mut v);
        v.finalize();
        v
    }

    /// Keep the better base for a slot, exactly as the old `upgrade` did.
    fn upgrade_ref(best: &mut BTreeMap<u32, (Tier, f32)>, slot: u32, tier: Tier, base: f32) {
        match best.entry(slot) {
            btree_map::Entry::Occupied(mut o) => {
                if base > o.get().1 {
                    o.insert((tier, base));
                }
            }
            btree_map::Entry::Vacant(e) => {
                e.insert((tier, base));
            }
        }
    }

    /// Exhaustive reference: every candidate the three passes can produce,
    /// every one of them scored, sorted and truncated. No heap, no capacity,
    /// no floor, no early-out, no grouping — nothing the selector may get
    /// wrong.
    ///
    /// Written per ENTRY rather than per arena, so it shares no code with the
    /// thing under test. Pass A is `find_iter` inside one record, which yields
    /// exactly the hits the arena-wide scan yields for that record: a match
    /// can never straddle a fence, and the resume point after a hit stays
    /// inside the record it was found in.
    fn reference_search(ix: &VolumeIndex, query: &str, max_results: usize) -> Vec<Hit> {
        let fq = fold(query);
        if fq.is_empty() || max_results == 0 || ix.is_empty() || has_control_byte(&fq) {
            return Vec::new();
        }
        let mut best: BTreeMap<u32, (Tier, f32)> = BTreeMap::new();

        // Pass A.
        for (slot, e) in ix.live_entries() {
            let rec = ix.folded_of_entry(e).as_bytes();
            for p in memmem::find_iter(rec, fq.as_bytes()) {
                let before = if p == 0 { FOLDED_DELIM } else { rec[p - 1] };
                let after = if p + fq.len() == rec.len() {
                    FOLDED_DELIM
                } else {
                    rec[p + fq.len()]
                };
                let tier = classify(before, after);
                upgrade_ref(&mut best, slot, tier, tier_base(tier));
            }
        }

        // Pass B, per entry rather than over the column: a query wider than a
        // lane cannot reach the tier at all (behavior change 2), and otherwise
        // a slot matches exactly when the folded query occurs inside its own
        // lane. That equivalence is what the real pass's byte-at-a-time resume
        // on a straddling hit buys — without it a rejected hit at a lane
        // boundary could skip a genuine match starting the next lane, and this
        // reference would be wrong rather than merely simpler.
        if fq.len() <= INITIALS_STRIDE {
            for (slot, e) in ix.live_entries() {
                let lane = initials_lane(ix.name_of_entry(e));
                if memmem::find(&lane, fq.as_bytes()).is_some() {
                    upgrade_ref(&mut best, slot, Tier::Initials, tier_base(Tier::Initials));
                }
            }
        }

        // Pass C, with NO prefilter at all — every live slot the higher tiers
        // did not claim is verified directly.
        //
        // That is the reference precisely because the real pass has one. The
        // class-mask prefilter is SOUND: if `fq` is a char-subsequence of a
        // folded name then every byte of `fq` occurs in it, so its class mask
        // is a superset and the slot survives the AND. So "survivors, verified"
        // and "everything, verified" have to agree exactly, and any class-map
        // or bit-slice bug — a bit set for the wrong slot, a bit not cleared on
        // delete, a restride that lost a slice — shows up here as a missing
        // row. Modelling the filter instead would only re-assert it.
        //
        // The trigram intersection this replaced could NOT be dropped from the
        // reference, because it had false negatives (behavior change 1): the
        // reference had to reproduce which genuine subsequences the prefilter
        // silently lost. That it can now be dropped is the change.
        //
        // `FUZZY_CAP` cannot bind at this corpus size, so the depth-bucketed
        // drain is a no-op here and the reference needs no notion of it.
        if fq.len() >= 3 {
            let qc: Vec<char> = fq.chars().collect();
            for (slot, e) in ix.live_entries() {
                if best.contains_key(&slot) {
                    continue;
                }
                if let Some(density) = fuzzy_density(ix.folded_of_entry(e), &qc) {
                    upgrade_ref(&mut best, slot, Tier::Fuzzy, 0.3 + 0.2 * density);
                }
            }
        }

        let mut all: Vec<Scored> = best
            .iter()
            .map(|(&slot, &(tier, base))| Scored {
                score: base * DEPTH_PEN[ix.rank_key[slot as usize] as usize],
                eidx: slot,
                tier,
            })
            .collect();
        all.sort_by(|a, b| b.cmp(a));
        all.truncate(max_results);
        all.iter()
            .map(|s| {
                let e = &ix.entries[s.eidx as usize];
                Hit {
                    frn: e.frn,
                    score: s.score,
                    match_ranges: compute_ranges(ix.name_of_entry(e), &fq, s.tier),
                }
            })
            .collect()
    }

    #[test]
    fn pruned_selector_matches_an_exhaustive_reference() {
        const CORPORA: u64 = 1500;
        const FILES: usize = 56;
        const PAGES: [usize; 6] = [1, 2, 3, 5, 8, 32];

        let mut compared = 0usize;
        let mut ties_at_boundary = 0usize;
        let mut nonempty = 0usize;
        for seed in 1..=CORPORA {
            let v = random_corpus(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15), FILES);
            for q in QUERIES {
                // The reference truncates a fully sorted candidate list, so its
                // answer for any K is a prefix of its answer for a larger one.
                // Computed once per query and sliced, which is what keeps the
                // corpus count in the thousands rather than the hundreds.
                let full = reference_search(&v, q, 33);
                for k in PAGES {
                    let got = v.search(q, k, &no_cancel);
                    let want = &full[..k.min(full.len())];
                    compared += 1;
                    nonempty += usize::from(!want.is_empty());
                    assert_eq!(
                        got.len(),
                        want.len(),
                        "seed {seed} query {q:?} k {k}: result count"
                    );
                    for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
                        assert_eq!(g.frn, w.frn, "seed {seed} query {q:?} k {k} row {i}: frn");
                        assert_eq!(
                            g.score.to_bits(),
                            w.score.to_bits(),
                            "seed {seed} query {q:?} k {k} row {i}: score"
                        );
                        assert_eq!(
                            g.match_ranges, w.match_ranges,
                            "seed {seed} query {q:?} k {k} row {i}: ranges"
                        );
                    }
                    // The property the strict rules exist for: the last row of
                    // a full page scoring exactly what the next candidate
                    // scores, so only the eidx tie-break separates them and a
                    // `≥` rule would have thrown the winner away.
                    if got.len() == k
                        && full.len() > k
                        && full[k].score.to_bits() == full[k - 1].score.to_bits()
                    {
                        ties_at_boundary += 1;
                    }
                }
            }
        }
        // The corpora are only a proof obligation if they actually exercise
        // one; assert the shape of the sample rather than trusting it.
        assert!(compared >= 50_000, "compared {compared}");
        assert!(
            nonempty * 4 > compared,
            "corpora barely match: {nonempty}/{compared}"
        );
        assert!(
            ties_at_boundary > compared / 20,
            "too few K-boundary ties to prove anything: {ties_at_boundary}/{compared}"
        );
    }

    #[test]
    fn fuzzy_upper_bound_is_the_density_ceiling_not_the_base_floor() {
        // `tier_base(Fuzzy)` is documented at its definition as a FLOOR: the
        // real base is 0.3 + 0.2·density, up to 0.5. Using it as a pruning
        // bound would discard every fuzzy candidate scoring 0.31-0.5.
        assert_eq!(tier_upper_bound(Tier::Fuzzy), 0.5);
        assert!(tier_upper_bound(Tier::Fuzzy) > tier_base(Tier::Fuzzy));
        // Every other tier's bound is its base, and the bounds are ordered.
        for t in [
            Tier::Exact,
            Tier::Prefix,
            Tier::WordBoundary,
            Tier::Initials,
            Tier::Substring,
        ] {
            assert_eq!(tier_upper_bound(t), tier_base(t));
        }
        assert!(tier_upper_bound(Tier::Substring) > tier_upper_bound(Tier::Fuzzy));
    }

    /// A rename to a SAME-LENGTH name must stop the old name matching.
    ///
    /// `arena_recs` is append-only, so the renamed slot's first record is still
    /// in it, still pointing at the bytes the old name occupied and still
    /// naming the slot. The only thing that tells the two records apart is
    /// [`slot_at`]'s `rec.off == entries[slot].folded_off` compare: length
    /// cannot, because a same-length rename leaves the containment arithmetic
    /// (`hit + qlen ≤ rec.off + folded_len`) bit-identical, and the slot cannot,
    /// because it is the same slot. Delete the compare and every offset inside
    /// the superseded record answers for the live entry again.
    #[test]
    fn same_length_rename_stops_matching_under_the_old_name() {
        let mut v = ix();
        v.add(1, 999, "alpha.txt", 0);
        v.add(2, 999, "gamma.txt", 0);
        let slot = v.frn_map[&1];
        let old = v.entries[slot as usize].folded_off;
        let old_len = v.entries[slot as usize].folded_len;

        v.apply(crate::UsnEvent::Rename {
            frn: 1,
            new_parent_frn: 999,
            new_name: "bravo.txt".into(),
        });
        let new = v.entries[slot as usize].folded_off;
        assert_eq!(
            v.entries[slot as usize].folded_len, old_len,
            "the rename must be same-length for this test to bite"
        );
        assert_ne!(old, new, "the rename appended a second record for the slot");

        // Black box: nothing of the old name is reachable, in any tier.
        for q in ["alpha.txt", "alpha", "lph", "pha.tx"] {
            assert!(v.search(q, 10, &no_cancel).is_empty(), "query {q:?}");
        }
        let hits = v.search("bravo", 10, &no_cancel);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].frn, 1);
        assert_eq!(v.search("gamma", 10, &no_cancel)[0].frn, 2);

        // White box, and this is the assertion the liveness compare is
        // load-bearing for. The superseded record is STILL in `arena_recs`,
        // still covering the old byte range, still naming the live slot — so
        // the mapping has to reject it on the offset compare alone. It is what
        // stands between "those bytes were NUL-filled" (invariant I4) and a
        // wrong result, for anything that later relaxes the erase or leaves a
        // record behind at compaction.
        let stale = v
            .arena_recs
            .iter()
            .find(|r| r.off == old)
            .expect("the superseded record is kept until compaction");
        assert_eq!(stale.slot, slot);
        for h in old as usize..old as usize + old_len as usize {
            assert_eq!(
                slot_at(&v.owner, &v.arena_recs, &v.entries, h, 1),
                None,
                "arena byte {h} still resolves to the renamed slot"
            );
        }
        assert_eq!(
            slot_at(&v.owner, &v.arena_recs, &v.entries, new as usize, 1),
            Some(slot),
            "the live record must still map"
        );
    }

    /// The hit → slot mapping at every offset a real scan can produce, on a
    /// churned arena, against an expectation built from the entry table alone.
    ///
    /// Two directions, and both matter. Every byte of a live record must
    /// resolve to that record's OWN slot — an off-by-one in `owner`'s fill rule
    /// or a missed record moves a hit to a neighbouring file, which no
    /// result-shaped assertion notices. And every byte of a vacated record must
    /// resolve to nothing, including after its slot has been handed to a new
    /// file off the freelist.
    ///
    /// Fence bytes are deliberately not probed: `slot_at`'s contract is a hit
    /// offset, and a delimiter-free needle can never start on a delimiter.
    #[test]
    fn slot_at_maps_live_bytes_and_rejects_vacated_ones() {
        const ABSENT: u64 = 9_000_000;
        /// Byte range the named entry is about to stop occupying.
        fn vacating(v: &VolumeIndex, frn: u64) -> (usize, usize) {
            let e = &v.entries[v.frn_map[&frn] as usize];
            (e.folded_off as usize, e.folded_len as usize)
        }

        let mut v = ix();
        for i in 0..300u64 {
            let pad = "y".repeat((i % 11) as usize);
            v.add(i + 1, ABSENT, &format!("doc-{i}-{pad}.md"), 0);
        }
        let mut vacated: Vec<(usize, usize)> = Vec::new();
        // Disjoint FRN sets: deletes take 6k+1 (odd), renames 4k+2 (even).
        for i in (0..300u64).step_by(6) {
            vacated.push(vacating(&v, i + 1));
            v.apply(crate::UsnEvent::Delete { frn: i + 1 });
        }
        for i in (1..300u64).step_by(4) {
            vacated.push(vacating(&v, i + 1));
            v.apply(crate::UsnEvent::Rename {
                frn: i + 1,
                new_parent_frn: ABSENT,
                new_name: format!("moved-{i}.md"),
            });
        }
        // Recycles the tombstoned slots, so a vacated record now names a slot
        // that belongs to a different file entirely.
        for i in 0..25u64 {
            v.add(50_000 + i, ABSENT, &format!("fresh-{i}.md"), 0);
        }
        v.finalize();
        assert!(
            v.live_entries().any(|(_, e)| e.frn >= 50_000),
            "the recycling creates must have landed"
        );

        let mut live_bytes = 0usize;
        for (slot, e) in v.live_entries() {
            for h in e.folded_off as usize..e.folded_off as usize + e.folded_len as usize {
                assert_eq!(
                    slot_at(&v.owner, &v.arena_recs, &v.entries, h, 1),
                    Some(slot),
                    "arena byte {h} of frn {}",
                    e.frn
                );
                live_bytes += 1;
            }
        }
        assert!(live_bytes > 2_000, "corpus too small to prove much");

        for &(off, len) in &vacated {
            for h in off..off + len {
                assert_eq!(
                    slot_at(&v.owner, &v.arena_recs, &v.entries, h, 1),
                    None,
                    "vacated arena byte {h} still resolves to a slot"
                );
            }
        }
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

    /// Assert every structural invariant the columns depend on. A violation
    /// here is the silent wrong-results mode: nothing panics, results just
    /// quietly become wrong for some slots.
    fn assert_invariants(v: &VolumeIndex, ctx: &str) {
        // Slot-keyed columns must all be exactly as long as the entry table,
        // or a slot indexes into the wrong occupant's data.
        assert_eq!(v.rank_key.len(), v.entries.len(), "{ctx}: rank_key length");
        assert_eq!(
            v.initials.len(),
            v.entries.len() * INITIALS_STRIDE,
            "{ctx}: initials length"
        );

        // The Pass B gate's whole soundness claim: the presence sets are a
        // SUPERSET of the initials column's n-grams. This is the direct check
        // — a miss here is a query the gate would skip that Pass B would have
        // answered, i.e. a silently missing result, which no timing or
        // result-diffing test can attribute.
        //
        // Restricted to n-grams a query could actually carry, which is the
        // property that matters and the only one that holds. `unregister_slot`
        // fills a lane with FOLDED_DELIM without recording it, and `compact`
        // refills from live lanes only — so on a column whose every live lane
        // is packed to all eight bytes, no NUL is ever recorded and a
        // subsequent delete would leave an unrecorded all-NUL lane behind.
        // That is harmless precisely because invariant I3 keeps bytes below
        // 0x20 out of every folded query, so those n-grams are unprobeable;
        // asserting over them would be a false alarm about a bit no gate
        // decision can ever read.
        for (slot, lane) in v
            .initials
            .as_chunks::<INITIALS_STRIDE>()
            .0
            .iter()
            .enumerate()
        {
            for &b in lane.iter().filter(|&&b| b >= 0x20) {
                assert!(
                    v.initials_may_contain(&[b]),
                    "{ctx}: slot {slot} byte {b:#04x} missing from the initials unigrams"
                );
            }
            for w in lane.windows(2).filter(|w| w.iter().all(|&b| b >= 0x20)) {
                assert!(
                    v.initials_may_contain(w),
                    "{ctx}: slot {slot} bigram {w:?} missing from the initials bigrams"
                );
            }
        }

        // `arena_recs` is sorted by construction (invariant I2) and is never
        // sorted at runtime, so a regression shows up as a broken ordering
        // rather than a slow sort.
        let mut prev: Option<u32> = None;
        for rec in &v.arena_recs {
            if let Some(p) = prev {
                assert!(rec.off > p, "{ctx}: arena_recs not strictly ascending");
            }
            prev = Some(rec.off);
        }

        // The FRN map holds exactly the live entries — no tombstone may remain
        // reachable, and no live entry may be missing.
        assert_eq!(v.len(), v.frn_map.len(), "{ctx}: live_count vs frn_map");
        let counted = v.entries.iter().filter(|e| !e.is_dead()).count();
        assert_eq!(counted, v.len(), "{ctx}: DEAD count vs live_count");

        // Fencing (invariant I3/I4): every live record's bytes are non-NUL and
        // every vacated record has been erased to NUL.
        let arena = v.folded_arena.as_bytes();
        assert_eq!(arena.first(), Some(&FOLDED_DELIM), "{ctx}: leading fence");
        assert_eq!(arena.last(), Some(&FOLDED_DELIM), "{ctx}: trailing fence");
        for (slot, e) in v.entries.iter().enumerate() {
            if e.is_dead() {
                continue;
            }
            let (off, len) = (e.folded_off as usize, e.folded_len as usize);
            assert!(
                arena[off..off + len].iter().all(|&b| b != FOLDED_DELIM),
                "{ctx}: live slot {slot} contains a delimiter"
            );
            assert_eq!(
                arena[off - 1],
                FOLDED_DELIM,
                "{ctx}: slot {slot} left fence"
            );
            assert_eq!(
                arena[off + len],
                FOLDED_DELIM,
                "{ctx}: slot {slot} right fence"
            );
        }
    }

    /// Step 10: the standing safety net.
    ///
    /// `pruned_selector_matches_an_exhaustive_reference` builds a fresh corpus
    /// per seed, so it never exercises MUTATION — and mutation is where the
    /// columns can silently disagree with `entries`. Slot recycling, arena
    /// erasure, the `arena_recs` liveness compare, the initials lane and the
    /// BSI bits all have to stay in step through delete/create/rename churn,
    /// and every one of them fails by returning the WRONG rows rather than by
    /// crashing.
    ///
    /// So: churn the index, then diff `search()` against the same exhaustive
    /// reference over the live set, and check the structural invariants after
    /// every round.
    #[test]
    fn churn_keeps_search_equal_to_the_reference() {
        const SEEDS: u64 = 60;
        const ROUNDS: usize = 10;
        const MUTATIONS: usize = 6;
        const PAGES: [usize; 3] = [1, 5, 32];

        let mut checked = 0usize;
        for seed in 1..=SEEDS {
            let mut rng = Rng::new(seed.wrapping_mul(0xD1B5_4A32_D192_ED03));
            let mut v = random_corpus(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15), 40);
            v.finalize();
            let mut next_frn = 500_000u64;

            for round in 0..ROUNDS {
                for _ in 0..MUTATIONS {
                    // Pick a live FRN to target, if any.
                    let live: Vec<u64> = v.frn_map.keys().copied().collect();
                    let victim = if live.is_empty() {
                        None
                    } else {
                        Some(live[rng.below(live.len())])
                    };
                    match rng.below(4) {
                        0 => {
                            next_frn += 1;
                            v.apply(crate::UsnEvent::Create {
                                frn: next_frn,
                                parent_frn: NO_PARENT,
                                name: random_name(&mut rng),
                                flags: 0,
                            });
                        }
                        1 => {
                            if let Some(frn) = victim {
                                v.apply(crate::UsnEvent::Delete { frn });
                            }
                        }
                        2 => {
                            if let Some(frn) = victim {
                                v.apply(crate::UsnEvent::Rename {
                                    frn,
                                    new_parent_frn: NO_PARENT,
                                    new_name: random_name(&mut rng),
                                });
                            }
                        }
                        // Same-FRN re-create: the update path, which tears the
                        // slot down and re-registers it.
                        _ => {
                            if let Some(frn) = victim {
                                v.apply(crate::UsnEvent::Create {
                                    frn,
                                    parent_frn: NO_PARENT,
                                    name: random_name(&mut rng),
                                    flags: 0,
                                });
                            }
                        }
                    }
                }

                let ctx = format!("seed {seed} round {round}");
                assert_invariants(&v, &ctx);

                for q in QUERIES.iter().take(12) {
                    let full = reference_search(&v, q, 33);
                    for k in PAGES {
                        let got = v.search(q, k, &no_cancel);
                        let want = &full[..k.min(full.len())];
                        checked += 1;
                        assert_eq!(got.len(), want.len(), "{ctx} query {q:?} k {k}: count");
                        for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
                            assert_eq!(g.frn, w.frn, "{ctx} query {q:?} k {k} row {i}: frn");
                            assert_eq!(
                                g.score.to_bits(),
                                w.score.to_bits(),
                                "{ctx} query {q:?} k {k} row {i}: score"
                            );
                        }
                        // Nothing may be returned that cannot be resolved.
                        for hit in &got {
                            assert!(v.name_of(hit.frn).is_some(), "{ctx}: unresolvable frn");
                        }
                    }
                }
            }
        }
        assert!(checked > 10_000, "too few comparisons to prove anything");
    }
}
