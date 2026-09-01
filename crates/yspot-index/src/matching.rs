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
//! initials are substring-scanned in their own small arena; fuzzy candidates
//! come from byte-trigram posting-list intersection, capped at
//! [`FUZZY_CAP`] scored candidates per query.
//!
//! `match_ranges` are UTF-16 code-unit ranges into the ORIGINAL (NFC) name
//! (§5.13), computed only for the final top-K page via [`fold_with_map`].

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap};

use memchr::memmem;

use crate::index::{
    fold, has_control_byte, ArenaRec, Entry, VolumeIndex, DEPTH_PEN, FOLDED_DELIM, OWNER_SHIFT,
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

/// Matcher acceleration structures, owned by the index behind a `Mutex` and
/// rebuilt lazily on the first search after any mutation (`dirty`).
///
/// Shrinking: the hit → slot map used to live here as a sorted
/// `(folded_off, slot)` vec that was rebuilt and re-sorted on the first query
/// after ANY mutation. It is now the index's own append-only
/// `arena_recs`/`owner` pair, maintained at the mutation choke points and never
/// rebuilt. Step 6 takes the initials arena the same way, and step 7 the
/// trigrams — at which point this type and its mutex go away entirely.
pub(crate) struct Accel {
    /// Set by index mutations; cleared by [`Accel::ensure_basic`].
    pub(crate) dirty: bool,
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
            initials_arena: String::new(),
            initials_spans: Vec::new(),
            trigrams: None,
        }
    }

    /// Rebuild the initials arena. Live entries only: a tombstoned slot keeps
    /// its ORIGINAL-case name in the name arena, so a dead slot left in the
    /// span table would map a hit back to a deleted file.
    fn ensure_basic(&mut self, ix: &VolumeIndex) {
        if !self.dirty {
            return;
        }
        self.initials_arena.clear();
        self.initials_spans.clear();
        self.initials_spans.reserve(ix.len());
        for (slot, e) in ix.live_entries() {
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
                entry: slot,
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
        // Live entries only (see `ensure_basic`), and ascending slot order —
        // the postings are intersected by binary search.
        for (slot, e) in ix.live_entries() {
            let bytes = ix.folded_of_entry(e).as_bytes();
            for w in bytes.windows(3) {
                let list = map.entry([w[0], w[1], w[2]]).or_default();
                if list.last().copied() != Some(slot) {
                    list.push(slot);
                }
            }
        }
        self.trigrams = Some(map);
    }

    /// Resident bytes per structure. Kept split rather than summed because
    /// the trigram postings are the largest single line in the whole index
    /// and a single total hides that (§4.3 attribution).
    pub(crate) fn ram_parts(&self) -> AccelRam {
        let trigrams = match &self.trigrams {
            // key + bucket + Vec header per posting list, plus list payloads.
            Some(tg) => {
                (tg.len() * (3 + 8 + std::mem::size_of::<Vec<u32>>())
                    + tg.values().map(|v| v.capacity() * 4).sum::<usize>()) as u64
            }
            None => 0,
        };
        AccelRam {
            initials: (self.initials_arena.capacity()
                + self.initials_spans.capacity() * std::mem::size_of::<InitialsSpan>())
                as u64,
            trigrams,
        }
    }
}

/// Per-structure resident bytes of [`Accel`]; folded into the index's
/// [`crate::index::RamBreakdown`].
pub(crate) struct AccelRam {
    pub initials: u64,
    pub trigrams: u64,
}

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
struct Selector {
    heap: BinaryHeap<Reverse<Scored>>,
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

impl Selector {
    fn new(max_results: usize, slots: usize) -> Self {
        let cap = max_results.saturating_mul(2).max(1);
        Self {
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
    fn offer(&mut self, rank_key: &[u8], slot: u32, tier: Tier, base: f32) {
        self.seen[slot as usize >> 6] |= 1 << (slot & 63);
        let s = Scored {
            score: base * DEPTH_PEN[rank_key[slot as usize] as usize],
            eidx: slot,
            tier,
        };
        if self.heap.len() < self.cap {
            self.heap.push(Reverse(s));
            if self.heap.len() == self.cap {
                self.floor = self.heap.peek().expect("full").0.score;
            }
        } else if s.cmp(&self.heap.peek().expect("full").0) == Ordering::Greater {
            // Equal here means same score AND same slot, i.e. a duplicate
            // offer, which drain-dedup would collapse anyway.
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

    let mut accel = ix.accel_lock();
    accel.ensure_basic(ix);

    let rank_key = &ix.rank_key;
    let mut sel = Selector::new(max_results, ix.entries.len());
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
    if !cancelled {
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

    // Pass B: camel/initials — substring scan of the initials arena.
    if !cancelled && !sel.rejects(tier_upper_bound(Tier::Initials)) {
        let mut pend: Option<u32> = None;
        for hit in finder.find_iter(accel.initials_arena.as_bytes()) {
            processed += 1;
            if processed.is_multiple_of(CANCEL_STRIDE) && is_cancelled() {
                cancelled = true;
                break;
            }
            // The floor only rises, so once the tier is out it stays out.
            if sel.rejects(tier_upper_bound(Tier::Initials)) {
                break;
            }
            let Some(slot) = initials_entry_at(&accel.initials_spans, hit, qlen) else {
                continue;
            };
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

    // Pass C: fuzzy subsequence over trigram-intersection candidates.
    // Queries shorter than 3 bytes skip the tier entirely (§3.4), and so does
    // a page already filled above the fuzzy ceiling.
    if !cancelled && qlen >= 3 && !sel.rejects(tier_upper_bound(Tier::Fuzzy)) {
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
                if sel.is_seen(cand) {
                    continue; // already offered by a higher tier (fuzzy max 0.5 < 0.55)
                }
                if !rest.iter().all(|l| l.binary_search(&cand).is_ok()) {
                    continue;
                }
                scored += 1;
                if scored > FUZZY_CAP {
                    break;
                }
                if scored.is_multiple_of(CANCEL_STRIDE) && is_cancelled() {
                    cancelled = true;
                    break;
                }
                let e = &ix.entries[cand as usize];
                if let Some(density) = fuzzy_density(ix.folded_of_entry(e), &fq) {
                    sel.offer(rank_key, cand, Tier::Fuzzy, 0.3 + 0.2 * density);
                }
            }
        }
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
    const WORDS: [&str; 6] = ["aa", "ab", "ba", "bb", "ac", "ca"];
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

        // Pass B, over the initials arena as `Accel::ensure_basic` lays it out,
        // straddle rejection included — this is the tier's candidate set as it
        // stands today, not as step 6 will rebuild it.
        let mut arena = String::new();
        let mut spans: Vec<(usize, usize, u32)> = Vec::new();
        for (slot, e) in ix.live_entries() {
            let name = ix.name_of_entry(e);
            let off = arena.len();
            for (seg, _) in segment_spans(name) {
                if let Some(c) = name[seg..].chars().next() {
                    arena.extend(c.to_lowercase());
                }
            }
            spans.push((off, arena.len() - off, slot));
        }
        for hit in memmem::find_iter(arena.as_bytes(), fq.as_bytes()) {
            if let Some(&(off, len, slot)) = spans.iter().rev().find(|s| s.0 <= hit) {
                if hit + fq.len() <= off + len {
                    upgrade_ref(&mut best, slot, Tier::Initials, tier_base(Tier::Initials));
                }
            }
        }

        // Pass C. The trigram prefilter is modelled directly: a posting list
        // for a trigram holds exactly the live slots whose folded name contains
        // it, so the intersection is "contains every 3-byte window".
        if fq.len() >= 3 {
            for (slot, e) in ix.live_entries() {
                if best.contains_key(&slot) {
                    continue;
                }
                let rec = ix.folded_of_entry(e);
                if !fq
                    .as_bytes()
                    .windows(3)
                    .all(|w| memmem::find(rec.as_bytes(), w).is_some())
                {
                    continue;
                }
                if let Some(density) = fuzzy_density(rec, &fq) {
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
}
