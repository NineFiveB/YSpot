# Accel-layer redesign (issues #1, #2, #3, #5)

Status: plan of record. Produced by three independent designs scored by two
adversarial judges, then synthesized. Implement in the step order below —
each step is independently testable and has an explicit gate.

## Chosen design

Design 1, "Slot-Stable Columns", as the spine — both judges picked it, and its core diagnosis is verified against the real code: `remove_entry` at index.rs:196 calls `entries.swap_remove(idx)`, which permutes entry indices and is the sole reason `ensure_basic` (matching.rs:80) must rebuild `folded_order` + the initials arena from scratch and null `trigrams`. Everything else follows from that one line.

Grafted, with attribution:
- STRICT `>` in every pruning rule + the differential-test proof obligation (Design 3). Non-negotiable correctness fix; Designs 1 and 2 both had it backwards.
- NUL-delimited folded arena (Design 2). Highest-value graft: tier classification from two L1-resident bytes with zero lookups, cross-record matches become structurally impossible, delete/rename reclaim arena bytes in place, and it retires the `find_iter` straddle bug for free.
- Inline running top-K with a tier-bound reject before any mapping (Design 2), replacing Design 1's per-tier buckets — which was incoherent, since buckets are filled by passes that have already run, so the "early-out skips whole passes" claim was structurally impossible.
- `rank_key: Vec<u8>` 1 B/entry column (Design 2) so the hot loop never touches the 32 MB `entries` array.
- Bit-sliced char-class index (Design 2) instead of a flat per-entry u64 signature: identical 8 B/entry, but a fuzzy query reads 0.5-1 MB of slices instead of sweeping 8 MB.
- `tri_present` / `bi_present` exact membership sets (Design 3) that skip the ARENA SCAN, not just the fuzzy tier. Replaces Design 1's `byte_hist`, which provably cannot fire on bench's `NO_MATCH_QUERIES` (z, q, x, j, v, w all occur in the corpus).
- Per-entry grouping of arena hits (Design 3), implemented via consecutive-slot tracking rather than a per-record rescan, so it costs nothing extra and preserves today's non-overlapping `find_iter` semantics exactly.
- Depth-bucketed shallowest-first FUZZY_CAP drain (Design 3).
- Capacity discipline as commit #1 and in-place-only compaction (Design 1 + Design 3), explicitly overruling Design 2's build-and-swap, which would peak ~218 MB/1M.

Rejected outright: Design 2's `parent_idx` replacing `parent_frn` (a wrong-RESULTS bug — delete a directory, recycle its slot, and every child's path is fabricated; `parent_frn` is self-validating and `path_of_missing_parent_anchors_at_root` depends on it), Design 2's Entry 32->24 relayout (unreachable once `parent_frn` stays; 8+8+4+4+4 = 28 -> 32), Design 3's global `dir_epoch` with query-path depth refresh (re-arms the exact rebuild-on-mutation cliff we are killing, on the query path).

## Why

The three measured failures have one root cause and two amplifiers, and the fix ordering follows from which is which.

ROOT CAUSE: `swap_remove` makes entry indices unstable, so no derived structure can be keyed by index, so `mark_dirty` -> `ensure_basic` is the only available repair. Tombstoning is the single change that unlocks incremental maintenance of every structure. Nothing else in the plan is possible before it.

AMPLIFIER 1 (latency): the ranking loop at matching.rs:586-604 calls `ix.depth_of(eidx)` per candidate — a parent-chain walk with an `frn_map` probe per level into a 20-35 MB map, ~250-400 ns — plus a SipHash insert into `best: HashMap<u32, Cand>`, plus `entry_at`'s `partition_point` over an 8 MB vec per raw hit (~360 ns cold). At ~40k candidates that is the whole 18 ms. Caching depth into a 1 MB `rank_key` column and replacing the map with a bitset takes per-candidate cost from ~800 ns to ~15-25 ns. That is the fix; the tier pruning is the safety valve for the tail, not the main mechanism.

AMPLIFIER 2 (memory): trigram postings are ~109 B/entry (back-derived: 270.9 warm minus ~136 cold minus 8 `folded_order` minus 12 `initials_spans` minus ~6 initials arena) — larger than `Entry` itself. An 8 B/entry bit-sliced character-class index is both smaller and a *sound* subsequence prefilter, where trigram intersection has false negatives (which is exactly why bench.rs must synthesize `plant_fuzzy_name` corpus members for the tier to fire at all).

Two things the judges caught that I have designed around rather than argued with:
1. Every pruning comparison must be strict `>`. `Scored::cmp` breaks equal scores by lower `eidx`, so `>=` discards tie-break winners. I use a 2K-capacity heap plus drain-dedup, which makes the reject provably result-preserving without any heap-position bookkeeping (proof in `final_design`).
2. Build-and-swap compaction breaches the 200 MB cap. All compaction is in-place leftward copy driven by one ascending pass over `arena_recs`, with the FRN table's *values* rewritten through a remap array rather than the table being rebuilt. Peak transient is 4.5 MB/1M, not a second copy of the index.

The design also closes open issue #3 as a consequence rather than as separate work: `Mutex<Accel>` exists only because `ensure_basic`/`ensure_trigrams` need `&mut Accel` during `search`. With nothing built lazily, `search(&self)` needs no lock and N sessions run in parallel under the existing `RwLock` read guard.

## Design

## 0. Invariants the whole design rests on

- **I1 — slot stability.** `entries[i]` belongs to one file for its lifetime. `remove_entry` tombstones (`flags::DEAD = 1<<3`) and pushes to `free_slots`; it NEVER calls `swap_remove`. Slots are recycled by new creates (safe here, unlike Design 2, because we keep `parent_frn` rather than `parent_idx`). Compaction is the only place slots are renumbered, and it rewrites every structure at once.
- **I2 — arenas are append-only in offset order.** Therefore `arena_recs` is sorted by construction; there is never a sort.
- **I3 — no byte < 0x20 in the folded arena except the record delimiters.** Enforced at `intern` (drop names whose folded form contains a control byte, alongside the existing empty-name drop) and at `search` (reject/strip control bytes from the folded query, which arrives from the untrusted pipe).
- **I4 — dead bytes never match.** Delete and rename NUL-fill the old folded record, zero the initials lane, and clear the entry's BSI bits. So the query hot path never needs a DEAD check; the `rec.off == entries[slot].folded_off` liveness compare is retained as a `debug_assert` and a defensive belt.

## 1. Entry layout — UNCHANGED

`Entry` stays exactly as it is: `{frn u64, parent_frn u64, name_off u32, folded_off u32, name_len u16, folded_len u16, flags u16}` = 30 B used, 2 B padding, 32 B, align 8. `parent_frn` is kept, so `path_of`, `depth_of`, and all five path tests (`path_of_chain`, `path_of_missing_parent_anchors_at_root`, `path_of_root_without_trailing_backslash`, `path_of_self_parent_terminates`, `path_of_cycle_is_finite`) are untouched. `flags` gains `DEAD = 1<<3` (DIR=1, HIDDEN=2, SYSTEM=4 are free of it; `attrs_to_flags` never sets it).

Ranking depth does NOT live in `Entry` — it lives in `rank_key`, because the hot loop must not touch a 32 MB array.

## 2. Folded arena becomes NUL-delimited

Layout: `0x00 rec0 0x00 rec1 0x00 rec2 0x00`. `intern` pushes `'\0'` then the record; the trailing NUL is maintained as an invariant so `arena[hit + qlen]` is always in bounds. `folded_off` points at the first byte of the record (after its NUL). `folded_arena` stays a `String` (U+0000 is valid UTF-8; record slices exclude the delimiters, so `folded_of_entry` is unchanged).

Consequences:
- Tier classification with ZERO lookups, from two bytes already in L1 from the scan: `arena[h-1]==0 && arena[h+qlen]==0` -> Exact; `arena[h-1]==0` -> Prefix; `is_sep_byte(arena[h-1])` -> WordBoundary; else Substring.
- A NUL-free query can never match across a delimiter, so no hit is ever rejected for straddling, so `find_iter`'s resume point can never skip a real match. The `"xa"`+`"aab"` / query `"aa"` bug is retired structurally.
- Delete/rename NUL-fill in place: `rebuild_after_delete_keeps_matches_consistent` becomes structurally true instead of incidentally true, and arena garbage is *inert* rather than merely bounds-checked.

Cost: +1 B per record.

## 3. Hit -> slot: `arena_recs` + `owner` (replaces `folded_order`)

```rust
struct ArenaRec { off: u32, slot: u32 }          // 8 B
arena_recs: Vec<ArenaRec>,                        // append-only, ascending in off by I2
owner: Vec<u32>,                                  // one u32 per 64 B of folded arena
```
`owner[b]` = index into `arena_recs` of the last record with `off <= b*64`. Maintained on append with `while owner.len()*64 <= folded_arena.len() { owner.push(arena_recs.len() as u32 - 1) }`.

Lookup, O(1) with no ordering assumption about slots:
```rust
let mut ri = owner[h >> 6] as usize;
while ri + 1 < arena_recs.len() && (arena_recs[ri+1].off as usize) <= h { ri += 1; }
let slot = arena_recs[ri].slot;
```
At ~24 B records a 64-byte block spans ~2.7 records, so the walk is <=3 steps. Because hits arrive in ascending offset order, both `owner` and `arena_recs` are read *sequentially*, so they stream — this is why they cost ~2 ns each and not a cache miss each. This is strictly better than Design 1's monotone cursor (which pays up to 8 MB of sequential advancement) and than Design 2's `folded_owner` (whose forward walk assumes entry-index order matches arena order — false after renames; Judge 2 was right).

`entry_at` and its `partition_point` are deleted.

## 4. Per-entry columns, all keyed by slot, all O(|name|) to maintain

```rust
rank_key: Vec<u8>,        // depth.min(127) | (hidden_or_system as u8) << 7    1 B/entry
initials: Vec<u8>,        // fixed stride 8, folded segment initials, NUL-pad  8 B/entry
charclass_bsi: Vec<u64>,  // 64 slices of ceil(n_slots/64) words               8 B/entry
```
- `DEPTH_PEN: [f32; 256]` static LUT holds `1/(1+0.02*d)` for `d = k & 0x7F`, pre-multiplied by `0.85` when bit 7 is set. Score = `base * DEPTH_PEN[rank_key[slot]]`: one L2 read + one L1 lookup + one multiply, replacing `ix.depth_of(eidx)`.
- `initials` lane `i` occupies `initials[8i..8i+8]`. Hit -> slot is `hit >> 3`; containment is `(hit & 7) + qlen <= 8`. No spans array, no `partition_point`. Zeroed on delete. NUL is never valid in a folded query (I3), so a match cannot straddle two lanes.
- `charclass_bsi`: `slice[c]` bit `i` is set iff slot `i`'s folded name contains a byte of class `c`. Class map, a pure function of the byte applied identically to arena and query: `b'a'..=b'z' -> 0..25`, `b'0'..=b'9' -> 26..35`, `-_. ` (space) `-> 36..39`, other ASCII `-> 40 + (b & 7)`, non-ASCII `-> 48 + (b & 15)`. Sound: if q is a char-subsequence of a name, every UTF-8 byte of q occurs in the name's UTF-8, so the name's class mask is a superset. Zero false negatives. Grow slices in 4 KiB chunks so allocator overshoot is bounded, not a doubling fraction.

Presence sets (fixed size, independent of n):
```rust
tri_present: [u64; 1<<18],   // 2^24 bits, EXACT membership over 24-bit byte trigrams   2.10 MB
bi_present:  [u64; 1<<10],   // 2^16 bits                                                8 KB
uni_present: [u64; 4],       // 256 bits                                                32 B

// The same question asked of the INITIALS COLUMN rather than the arena, which
// is what gates Pass B (§6). Separate sets are required, not a convenience:
// the arena's bigrams say nothing about which letters are adjacent as segment
// initials. N-grams are taken per lane, over the whole padded 8 bytes.
initials_bi_present:  [u64; 1<<10],   // 2^16 bits over lane bigrams                     8 KB
initials_uni_present: [u64; 4],       // 256 bits over lane bytes                       32 B
```
Set-only (deletes do not clear); a stale bit costs one wasted scan, never a wrong result; rebuilt at compaction. Trigrams are inserted per-record (never across a delimiter). Note this is an *exact* 24-bit membership set, not a Bloom filter — the only false positives come from dead entries. `tri_present` is the one set that is conditionally allocated (only once the arena it accelerates exceeds its own 2 MiB); the two 8 KiB pairs break even around 1k entries, two orders of magnitude earlier, and are always held.

`initials_arena`, `initials_spans`, `InitialsSpan`, `initials_entry_at`, `trigrams`, `ensure_trigrams`, `ensure_basic`, `Accel`, `dirty`, `accel_lock`, `mark_dirty`, `Mutex<Accel>`, `best: HashMap<u32,Cand>`, `Cand`, `upgrade` are all deleted.

## 5. Incremental maintenance — two choke points

All mutation goes through `fn register_slot(&mut self, slot, frn, parent_frn, name, flags)` and `fn unregister_slot(&mut self, slot)`. Nothing else writes a column. `debug_assert` on column lengths at both.

**Create** (and `add_entry`'s existing-FRN update path, index.rs:166-173, which behaves as a rename — Judge 1 correctly flagged Design 1 for leaving this unspecified): intern (NUL + NFC + folded, O(|name|)); `slot = free_slots.pop().unwrap_or(entries.len())`; `arena_recs.push`; extend `owner`; `rank_key[slot] = (parent_depth+1).min(127) | hidden<<7`; set the initials-lane presence bits, then write the 8-byte `initials` lane (allocation-free segment walk) — bits before bytes, so the superset invariant holds at every intermediate point rather than only at function exit; set BSI bits for the <=12 distinct classes; set trigram/bigram/unigram presence bits; `frn_map.insert`; `live_count += 1`. ~1-3 us. **Nothing is rebuilt, nothing is sorted, nothing is invalidated — this is the entire fix for the 167 ms cliff.**

**Delete**: `frn_map.remove`; read the still-live folded name and clear BSI bits for its distinct classes (exact, ~12 scattered writes — NOT optional, since slots are recycled and a stale union degrades selectivity monotonically); NUL-fill the folded record; zero the initials lane (the initials presence bits are NOT cleared, for the same reason the arena's are not: they are global and unkeyed, and the sets are only ever a negative test, so correctness needs a superset of the live column — which deletion preserves trivially, since it only removes content); `flags |= DEAD`; `dead_bytes += name_len + folded_len`; `free_slots.push(slot)`; `live_count -= 1`. O(|name|).

**Rename / same-FRN Create**: delete-side bookkeeping for the old name, then create-side for the new, keeping the same slot. Old `arena_recs` entry self-invalidates (`rec.off != entries[slot].folded_off`) and its bytes are NUL-filled.

**Depth.** `finalize()` runs a memoized, cycle-guarded O(n) sweep computing every entry's depth from its `parent_frn` chain and writing it into `rank_key`. Required because `FSCTL_ENUM_USN_DATA` walks in MFT-record order, not parent-first, so a child seen before its parent would cache 0. Called via a new `EntrySink::finish()` default method from `mft::enumerate` and `walk::walk`, and from bench after its insert loop. ~20-40 ms/1M, inside the SPEC 3.2 15 s budget.

**Directory moves.** A `Rename` that changes `parent_frn` on a DIR leaves descendants' cached depth stale by a constant offset. We do NOT do what Design 3 did (lazy refresh on the query path — that re-arms the cliff inside the 10 ms budget). Instead: set `depth_repair_pending`, and run the memoized sweep **on the writer thread**, debounced ~500 ms, sliced at 64 Ki entries per write-lock acquisition. The query path reads `rank_key` unconditionally with no epoch check and no fallback walk. Cost of staleness: a few percent of score on a moved subtree, for a few hundred ms. No result appears or disappears. `depth_of()` itself keeps exact walking semantics for `path_of` and the public API.

## 6. Query path

Preflight, O(|q|): `fq = fold(query)`; reject if it contains a byte < 0x20 (I3); check `uni_present`/`bi_present`/`tri_present`.

**Selector.** A min-heap of capacity `2*max_results` holding `Scored`, plus a `seen` bitset of n bits (125 KB/1M, `vec![0u64; ...]`, lazily-zeroed pages, ~10-20 us).

*Why 2K and why it is exactly right:* an entry can be offered at most twice — once by Pass A (all its hits are inside its one contiguous record, so consecutive-slot grouping collapses them) and once by Pass B; Pass C skips `seen` slots (fuzzy base <= 0.5 < 0.55 <= any Pass A tier, < 0.7 = Pass B tier, and the depth/hidden factors are identical for the same slot, so a Pass C offer for a seen slot is strictly worse). Let `s_K` be the K-th best distinct slot with best score `v`. Each of the K-1 better distinct slots contributes at most 2 offers, so `(v, s_K)` ranks at worst 2K-1 among all offers, hence is retained by a 2K heap. Drain, dedup by slot keeping the max, sort descending, truncate to K — identical to today's result set and order, with no heap-position bookkeeping.

**Pruning rules, both STRICT.**
- `tier_upper_bound(t)`: Exact 1.0, Prefix 0.9, WordBoundary 0.8, Initials 0.7, Substring 0.55, **Fuzzy 0.5**. This is a NEW function, distinct from `tier_base`, because `tier_base(Fuzzy)` returns 0.3 (matching.rs:255, "floor; actual fuzzy base is density-scaled") and using it as a bound would drop fuzzy candidates scoring 0.31-0.5.
- **Pass skip:** skip a pass iff the heap is full at 2K AND `heap_min.score > tier_upper_bound(t)`. Strict `>`, because on equal score a lower `eidx` wins.
- **Hit reject:** when the heap is full, reject iff `tier_upper_bound(t) < heap_min.score` (strict). On equality, fall through to the mapping and the full `Scored::cmp`.
- The running floor is the 2K-th best over a *prefix* of the offer stream, hence <= the final 2K-th best, so both rules are conservative. Sound.

**Pass A — one memmem over the folded arena.** Driven manually so the cursor is controllable:
```
pos = 0; pend_slot = NONE; pend_tier = NONE;
loop {
  h = pos + finder.find(&arena[pos..])?;            // ascending, non-overlapping
  t = classify(arena[h-1], arena[h+qlen]);          // 2 L1 bytes, zero lookups
  if full && ub(t) < floor { pos = h + qlen; continue; }   // ~5 ns, touches nothing
  slot = owner/arena_recs lookup;                   // streaming, ~4 ns
  if slot != pend_slot { flush(pend); pend_slot = slot; pend_tier = t; }
  else { pend_tier = max(pend_tier, t); }
  pos = h + qlen;
}
flush(pend);
```
`flush` computes `base * DEPTH_PEN[rank_key[slot]]`, offers to the heap, sets `seen[slot]`. Consecutive-slot grouping gives Design 3's per-entry semantics (max base per entry, exactly what `upgrade` did) with no memchr and no rescan. Advancing by `qlen` preserves today's non-overlapping `find_iter` semantics exactly, so no behavior changes on that axis.

Poll `is_cancelled` every `CANCEL_STRIDE` hits AND every 1 MiB of arena advanced, so a zero-hit 24 MB scan is interruptible.

**Pass B — initials.** Skipped if `fq.len() > 8` (cannot fit a lane — this is what keeps the `exact` class from regressing on the wider fixed-stride array), if `floor > 0.7`, or if `initials_bi_present`/`initials_uni_present` miss. `memmem` over the stride-8 array; `slot = hit >> 3`; `(hit & 7) + qlen <= 8`; same flush path.

CORRECTED (issue #8). This section originally prescribed gating Pass B on `bi_present`, the ARENA's bigram set. That gate is unsound: the arena's bigrams describe which bytes are adjacent inside whole folded names, which says nothing about which letters are adjacent as segment INITIALS — `Foo Bar.txt` carries the lane bigram `fb` while its folded name does not contain `fb` anywhere. The implementation correctly refused the gate and, having no replacement, left Pass B scanning the entire 8 MB column for every query of 8 bytes or fewer. The fix is a set over the initials column's own lane bytes, sound for the same reason `bi_present` is sound for Pass A: the containment test `(hit & 7) + qlen <= 8` forces a match to lie wholly inside one lane, so every byte and bigram of a real Pass B match is a byte and bigram of some single lane. N-grams are taken over the whole padded lane rather than its live prefix — the extra bits all involve the NUL pad, and no query can carry a byte below `0x20`, so the gate's answers are unchanged while the rule stops depending on the lane's packing discipline. Both sets are set-only and refilled at compaction, exactly like the arena's. Ordered AFTER the `floor > 0.7` test, which is cheaper and in practice more selective.

Measured at 1M, 200 iterations/class: `no-match` p50 274 -> 95 us. At 300k: 65 -> 11 us p50, p95 ~20-36 us — still several times the ~5 us p95 this design predicted for that path at 300k (see the prediction table below), so the prediction stands unmet, not met. No other class moves outside run-to-run noise. That is the expected shape: a presence gate can only skip a pass whose OWN column cannot carry the query, and for `initials-2`/`initials-3` the queries are harvested from real lanes, so they match, the gate passes, and the column is scanned as it must be. Note the gate is per-column, not per-query: a query the initials sets reject can still be answered by another tier, which `an_initials_miss_does_not_suppress_a_substring_hit` pins.

Issue #8's second claim — that this also removes "a floor under every short query: 590 us of the `initials-2` class at 1M" — is NOT addressed here and cannot be, for the reason above. That floor was `Selector::offer` evaluating the caller's name filter on every candidate before scoring it; fixed separately in the preceding commit, which is where `initials-2` improves.

**Pass C — fuzzy.** Gated on `fq.len() >= 3` (unchanged, SPEC 3.4) and `floor > 0.5`. Note `tri_present` gates ONLY Pass A — fuzzy does not require contiguity, so gating Pass C on it would be wrong. Stage 1: AND the 4-8 BSI slices the query's distinct classes need (0.5-1 MB read, not an 8 MB sweep), extract survivors with `trailing_zeros`, skip `seen`, and drop any whose `rank_key` depth exceeds `d_max = ((0.5/floor - 1)/0.02)` when the heap is full. Stage 2: drain depth buckets **shallowest first**, verifying with an allocation-free `fuzzy_density`, capped at `FUZZY_CAP` verified candidates. Because fuzzy base <= 0.5 regardless of density, shallow candidates dominate, so the cap drops exactly the candidates least able to reach the page — a defensible rule, unlike today's "first 20,000 in arena order".

`fuzzy_density` is rewritten allocation-free: it currently collects TWO `Vec<char>` per candidate (matching.rs:311-312) — 40,000 allocations per query at the current cap. Forward pass over `char_indices()`, backward pass over `name[..=end].char_indices().rev()`, query pre-collected once per search into a stack `[char; 64]` buffer.

`compute_ranges` / `fold_with_map` / `utf16_end` are untouched; still computed only for the final page, so SPEC 5.13 UTF-16 offsets into the original NFC name are preserved exactly.

## 7. Concurrency — closes issue #3 by deletion

With nothing built lazily there is nothing for `search` to mutate. The former `Accel` fields become plain `VolumeIndex` fields written only through `&mut self` in `add_entry`/`remove_entry`/`rename_entry`. `search(&self)` takes no lock; `ram_bytes()` takes no lock. The outer `RwLock<VolumeIndex>` in `state.rs` becomes the only lock and searches hold it for read, so N sessions search in parallel on N cores (SPEC 2.3). `VolumeIndex` stays `Sync` (only `Vec`/`String`/`HashMap`, no interior mutability — deliberately not Design 3's `Vec<AtomicU64>` sidecar). Per-query scratch (2K heap, `seen` bitset, depth buckets) is call-local.

Writers apply USN events in batches, one write-lock acquisition per journal read batch; at ~1-3 us/event a 1000-event batch is a sub-millisecond stall.

## 8. Compaction — IN PLACE ONLY

Triggers: `dead_bytes > 25%` of live arena, OR stale `arena_recs > 25%`, OR `dead_slots > 12.5%`, OR the SPEC 3.7 15-minute snapshot cadence. Machine-idle gated per SPEC 3.6, with a hard threshold (40% garbage) that forces it outside an idle window rather than letting the structures grow without bound.

One ascending pass over `arena_recs` drives everything, because live records appear in `arena_recs` in ascending append order and both arenas were appended in that same order — so live `name_off`s are ascending in `arena_recs` order too, and both arenas compact with `copy_within` leftward, write cursor <= read cursor, ZERO transient. Then: truncate + `shrink_to_fit`, renumber slots densely, rebuild `owner`/columns/presence sets in place, rewrite the FRN table's *values* through a `remap: Vec<u32>` (the KEYS are unchanged, so hash positions are unchanged — no table rebuild, no second map), re-run the depth sweep, drain `free_slots`/`dead_bytes`. Peak transient = the 4.5 MB remap array.

**A code comment on `compact()` must state the rule and the arithmetic: never build-and-swap, never allocate a second arena or a second FRN table.** Design 2 walked into exactly this and would peak at ~218 MB/1M. Write-lock hold is ~150-350 ms at 1M; set `VS_REBUILDING` and bump `index_epoch` via the existing `state.rs` machinery.

## 9. Service-path fix (outside the matcher, but it will dominate once the matcher is fixed)

`session.rs:269-273` sets `fetch = max*8` clamped to 4096 whenever an ext or path filter is present. That multiplies `compute_ranges`/`fold_with_map` (a `String` + `Vec<u32>` allocation each) by up to 128x, and it inflates the pruning K to 8192, which keeps the floor low far longer and weakens every skip. Fix: have `search` return `(frn, score, tier, slot)` and expand `match_ranges` only for items that survive the filters; and push the ext filter into `flush` as a cheap name-suffix predicate so `fetch == max` again.

## Byte accounting

> **Superseded parameter (issue #10, M1).** Sections A–D below are computed at
> L = 23 B, which was derived, not measured. Real volumes measure L = 30–35 B
> (§F). Every "headroom" figure in A–D is therefore optimistic by ~15–25
> B/entry; §F restates the totals at the measured range and revisits Step 12.

Parameters. n = 1,000,000 live entries. L = **23 B** average name, used for BOTH arenas.

Derivation of L (the judges split 17 vs 26; neither is derived): the measured cold figure is 188,743,680 B at 1,329,180 entries. `ram_bytes` computes `entries.capacity()*32 + name.capacity() + folded.capacity() + frn_map.capacity()*16`. At 1.33M: `entries` capacity 2^21 -> 67,108,864 B; `frn_map.capacity()` = 1,835,008 -> x16 = 29,360,128 B. Residual for BOTH arenas = 92,274,688 B of **capacity** = 34.7 B/entry each. `String` grows by doubling, so mean slack over the doubling interval is ~1.5x, giving ~23 B/entry of payload each. That is a derivation, not a guess, and it sits between the two disputed figures.

### A. Post-compaction steady state, per 1M live entries

| # | structure | B/entry | MB/1M |
|---|---|---:|---:|
| 1 | `entries: Vec<Entry>` (32 B, unchanged layout) | 32.00 | 32.00 |
| 2 | `name_arena` (23 B) | 23.00 | 23.00 |
| 3 | `folded_arena` (23 B + 1 NUL delimiter) | 24.00 | 24.00 |
| 4 | `frn_map: HashMap<u64,u32>` — 2^21 buckets x (16 B payload + 1 ctrl) | 35.65 | 35.65 |
| 5 | `arena_recs: Vec<ArenaRec>` 8 B @ 1.0 rec/entry | 8.00 | 8.00 |
| 6 | `owner: Vec<u32>`, 1 per 64 B of folded arena (24 MB / 64) | 1.50 | 1.50 |
| 7 | `initials: Vec<u8>` stride 8 | 8.00 | 8.00 |
| 8 | `rank_key: Vec<u8>` | 1.00 | 1.00 |
| 9 | `charclass_bsi` 64 x ceil(n/64) x 8 B | 8.00 | 8.00 |
| 10 | `tri_present` 2^24 bits | 2.10 | 2.10 |
| 11 | `bi_present` + `uni_present`, and the same pair over the initials column | 0.02 | 0.02 |
| 12 | `free_slots` (empty post-compaction) | 0.00 | 0.00 |
| 13 | fixed-chunk allocation slack (2 MiB entries + 2 x 1 MiB arenas) | 4.19 | 4.19 |
| | **TOTAL — Phase A (std HashMap kept)** | **147.46** | **147.46** |
| | replace row 4 with open-addressed `frn_index` (2^21 slots x 4 B = 8.39) | -27.26 | -27.26 |
| | **TOTAL — Phase B (Step 11 landed)** | **120.20** | **120.20** |

Sum check A: 32.00+23.00=55.00; +24.00=79.00; +35.65=114.65; +8.00=122.65; +1.50=124.15; +8.00=132.15; +1.00=133.15; +8.00=141.15; +2.10=143.25; +0.02=143.27; +0.00=143.27; +4.19 = **147.46**. Phase B: 147.46 - 35.65 + 8.39 = **120.20**.

- Phase A = **73.7% of the 200 MB hard cap** (52.5 MB headroom), 22.9% over the 120 MB typical target.
- Phase B = **60.1% of the cap**, and lands **0.2% over the 120 MB typical target** — at the line.
- **The budget closes under the hard cap without the risky hand-rolled table.** That is the deliberate risk posture both judges asked for: stage the ~120-line open-addressed table last and drop it if the schedule tightens.
- Versus the measured 270.9 B/entry warm at 300k: **1.84x reduction (Phase A) / 2.25x (Phase B)**.

### B. At the compaction trigger (25% stale arena bytes AND 25% stale recs AND 12.5% dead slots, all at once)

| structure | multiplier | MB/1M |
|---|---|---:|
| entries | x1.125 | 36.00 |
| name_arena | x1.25 | 28.75 |
| folded_arena | x1.25 | 30.00 |
| frn_map (live-sized, unaffected) | — | 35.65 |
| arena_recs | x1.25 | 10.00 |
| owner (scales with folded arena) | x1.25 | 1.88 |
| initials | x1.125 | 9.00 |
| rank_key | x1.125 | 1.13 |
| charclass_bsi | x1.125 | 9.00 |
| tri_present | — | 2.10 |
| bi/uni_present, arena and initials column | — | 0.02 |
| free_slots (125k x 4 B) | — | 0.50 |
| allocation slack | — | 4.19 |
| **TOTAL at trigger — Phase A** | | **168.22** |
| **TOTAL at trigger — Phase B** | | **140.96** |

Sum check: 36.00+28.75=64.75; +30.00=94.75; +35.65=130.40; +10.00=140.40; +1.88=142.28; +9.00=151.28; +1.13=152.41; +9.00=161.41; +2.10=163.51; +0.02=163.53; +0.50=164.03; +4.19 = **168.22**. Phase B: 168.22 - 35.65 + 8.39 = **140.96**.

Phase A at trigger = 84.1% of cap. Phase B at trigger = 70.5% of cap. (Design 1 omitted this transient entirely — Judge 1's flaw #3.)

### C. Peak during compaction

In-place leftward copy, so the arenas and `arena_recs` need no second copy, and the FRN table's values are rewritten through a remap rather than the table being rebuilt. Only transient = `remap: Vec<u32>` sized by pre-compaction slots = 1.125M x 4 B = **4.50 MB**.

- **Phase A peak = 168.22 + 4.50 = 172.72 MB/1M (86.4% of cap).**
- **Phase B peak = 140.96 + 4.50 = 145.46 MB/1M (72.7% of cap).**

For contrast: Design 2's background build-and-swap would peak at ~218 MB/1M, and a naive "rebuild the frn_map" step inside compaction would push Phase A to 203.9 MB — over the cap. Both are forbidden in a comment on `compact()`.

### D. Sensitivity to L (the one parameter I cannot measure from here)

Rows 2, 3, and 6 scale with L; nothing else does. d(total)/dL = 2 + 1/64*... ~= 2.06 B/entry per byte of L.

| L | Phase B post-compact | Phase B at trigger | Phase A at trigger | Phase A peak |
|---|---:|---:|---:|---:|
| 20 | 114.0 | 133.2 | 160.4 | 164.9 |
| **23 (planned)** | **120.2** | **141.0** | **168.2** | **172.7** |
| 26 (Design 2's estimate) | 126.4 | 148.7 | 175.9 | 180.4 |
| 30 | 134.6 | 158.9 | 186.1 | 190.6 |
| 35 | 144.9 | 171.8 | 199.0 | 203.5 |

**Phase A stays under the 200 MB cap up to L ~= 34 B; Phase B up to L ~= 63 B.** So even at Design 2's more pessimistic 26 B the plan holds with room, and Phase B is robust to any realistic filename distribution. If Step 0's real-volume measurement puts L above 30, land Step 11 before Step 9.

### E. Where the ~123-151 B/entry goes (270.9 measured -> 120.20)

| delta | source |
|---:|---|
| **-109.0** | trigram postings deleted (~109 B/entry, back-derived) -> `charclass_bsi` at 8.00: net **-101.0** |
| -12.0 | `initials_spans` deleted |
| -8.0 | `folded_order` deleted |
| -6.0 | `initials_arena` -> fixed 8 B lane: net **+2.0** |
| **-36.0** | capacity overshoot removed (`shrink_to_fit` + fixed-chunk `reserve_exact`). Largest single win, five lines, zero behavior change |
| -27.3 | `frn_map` -> open-addressed `frn_index` (Phase B only) |
| +8.00 | `arena_recs` (replaces `folded_order`; +1.5 for `owner`, buying O(1) lookup and zero rebuild) |
| +1.00 | `rank_key` |
| +1.00 | folded-arena NUL delimiters |
| +2.11 | presence sets, arena and initials column (fixed size) |

### F. Prerequisite: `ram_bytes()` is currently wrong

index.rs:307 computes `frn_map.capacity() * 16`. hashbrown stores `(u64,u32)` = 16 B **plus one control byte per BUCKET**, and `capacity()` reports 7/8 of buckets. At 1M that is 29.36 MB reported against 35.65 MB real — a **21% understatement**. Every budget line above is measured through this function, so fixing it is a prerequisite for the 120/200 MB lines being CI-enforceable at all. Correct formula: `buckets = (capacity()*8/7).next_power_of_two().max(8); buckets * 17`. Also add a per-structure breakdown so `IndexStatus.ram_bytes.filename` (SPEC 4.3) is attributable. This lands in Step 0, before anything else, so the memory work is measured against a truthful baseline.

## Expected results, and what would falsify this design

## Predicted numbers

All predictions are for `max_results = 32` and are **lower bounds on the real service** for the same reason bench.rs's header says so: no pipe I/O, no serialization, no SPEC 3.8 AccessCheck. SPEC 2.5 allots 2 ms of the 10 ms to trimming, so treat **8 ms as the working budget**.

### At 300k entries (directly comparable to the measured failures)

| class | measured p95 | predicted p95 | predicted max | mechanism |
|---|---:|---:|---:|---|
| **initials-2** | 17,828 us FAIL | **1,400-2,200 us** | ~3,500 us | cached depth kills the `depth_of` walk; when the 2K heap fills from prefix hits the floor reaches ~0.849 > 0.7 and the whole initials scan is skipped |
| **substring** | 11,569 us (max 84,622) FAIL | **600-1,000 us** | **~2,000 us** | the 84.6 ms max was per-hit `partition_point` + per-candidate `depth_of`; both gone. p95-to-max spread collapses from 7.3x to ~2x |
| **common-substr** | 18,408 us FAIL | **900-1,500 us** | ~2,500 us | floor reaches ~0.74 from prefix/word-boundary hits; every Substring hit is then rejected in ~5 ns without touching memory |
| **post-mutation 2-char** | 57,402 us | **~1,500 us** (= initials-2 mean) | | nothing is rebuilt |
| **post-mutation 3-char** | 167,233 us | **~1,200 us** (= initials-3 mean) | | nothing is rebuilt |
| exact | 305 us pass | ~300 us (neutral) | | Pass B skipped when `fq.len() > 8`, which is what prevents a regression from the wider fixed-stride initials array |
| prefix | 8,057 us marginal | ~500 us | | |
| word-boundary | 7,906 us marginal | ~500 us | | |
| initials-3 | 962 us pass | ~700 us | | |
| fuzzy-subseq | 318 us pass | ~350 us (neutral) | | BSI slice-AND replaces posting-list intersection |
| no-match | 192 us pass | **~5 us** | | `tri_present` miss skips Pass A entirely, which `byte_hist` provably could not do (z/q/x/j/v/w all occur in the corpus) |
| `apply(Create)` | not measured | **1-3 us** | | new bench row |

### At 1M entries (the real target; folded arena = 24 MB)

| class | predicted p95 | margin vs 8 ms working budget |
|---|---:|---|
| exact / prefix / word-boundary | 1.2-2.5 ms | comfortable |
| **initials-2** | **4.5-6.5 ms** | **1.5-3.5 ms — thinnest class, the residual risk** |
| initials-3 | 2.5-3.5 ms | comfortable |
| substring | 2.0-3.0 ms | comfortable |
| **common-substr** | **3.5-5.5 ms** | 2.5-4.5 ms |
| fuzzy-subseq | 0.9-1.5 ms | comfortable |
| no-match | < 10 us | — |
| post-mutation (any) | = steady state | — |

Cost model behind these (per candidate, replacing today's ~800 ns): reject-without-mapping ~5 ns (two L1 bytes + one float compare); map ~4 ns (`owner` and `arena_recs` are read in ascending order, so they *stream* rather than miss); flush ~15 ns (one random read into the 1 MB `rank_key`, one L1 LUT, one multiply, one heap compare). The irreducible floor is the memmem scan itself: ~4.0 ms for a 2-byte needle over 24 MB, ~2.4 ms for 4 bytes, ~0.8 ms for 6+ bytes with a rare byte.

## What would falsify this design

1. **After Step 4 alone** (rank_key + inline top-K + cached depth, no structural change), if initials-2 / substring / common-substr p95 at 300k are not below ~4 ms, the per-candidate cost model is wrong and the bottleneck is the scan rather than ranking. Escalate to Step 12. **This is the single most important early gate — it is why Step 4 lands before any structure is replaced.**
2. **Step 0's isolated scan probe at 1M** (a long zero-hit needle that passes `tri_present`, timing Pass A alone): if the folded-arena scan by itself exceeds ~4 ms, SPEC 3.4's "a 1M-name arena scans in single-digit ms" premise fails for us and no amount of per-candidate work removes it. Escalate to Step 12 (2-byte prefix/exact CSR index, +4.3 MB/1M, which lets the tier skip fire *before* Pass A runs; and/or chunked parallel scan across 2-4 threads with `qlen-1` overlap and per-thread heap merge — embarrassingly parallel, no new dependency).
3. **Post-mutation is not within 10% of steady state** for the same query -> something is still being rebuilt; find it before proceeding.
4. **`ram_bytes` at 1M post-compaction exceeds 135 MB (Phase B) / 160 MB (Phase A)** -> the L = 23 B derivation is wrong. Re-derive from the real volume; the cap holds to L ~= 34 (Phase A) / 63 (Phase B), so this is a re-plan, not a redesign, but it moves Step 11 earlier.
5. **The differential test finds ANY result difference attributable to pruning** -> the strict-`>` / 2K-heap reasoning is wrong and both must come out until the proof is repaired. This is the only thing that actually proves the pruning is result-preserving; the 81 existing tests cannot (`max_results_caps_and_orders` only asserts descending order and has no K-boundary tie).
6. **BSI survivor counts on the real 1.33M volume exceed ~50k for a 6+ char query** -> the fuzzy prefilter's selectivity assumption fails and FUZZY_CAP truncation becomes visible as missing results rather than latency. Fix: a second 64-class slice keyed on character PAIRS present (+8 B/entry; the budget has room to 200 MB in both phases).
7. **The concurrent-search bench class does not scale near-linearly to 4 threads** -> something still serializes; issue #3 is not actually closed.
8. **Compaction peak exceeds trigger + 6 MB/1M** (instrumented with a counting allocator in tests) -> someone reintroduced a build-and-swap somewhere.

## Implementation steps

0. STEP 0 - Truthful instrumentation, zero behavior change. Fix `ram_bytes()` (index.rs:307): `frn_map.capacity()*16` understates hashbrown by 21% (16 B payload + 1 control byte per BUCKET, capacity() = 7/8 of buckets); add a per-structure breakdown so SPEC 4.3's `ram_bytes.filename` is attributable. Extend bench.rs: `--entries 1000000`, p99 alongside max, an isolated Pass-A scan probe (long zero-hit needle) so the memmem floor is measurable on its own, and a per-structure ram table. GATE: all tests pass unchanged; `ram_cold`/`ram_warm` rise ~20% (that is the bug being fixed, not a regression). Everything after this is measured against a truthful baseline.

1. STEP 1 - Capacity discipline. Add `VolumeIndex::finalize()` doing `shrink_to_fit()` on `entries`, both arenas and `frn_map`; afterwards grow with `reserve_exact` in fixed chunks (64 Ki entries / 1 MiB arena) instead of geometric doubling. Add `EntrySink::finish()` as a default no-op, implement it on VolumeIndex as `finalize()`, and call it at the end of `mft::enumerate` and `walk::walk`; call it in bench after the insert loop. GATE: all tests pass with no edits (`walk.rs`'s `Collect` sink compiles via the default method); bench `ram_cold` drops ~30-35% (~142 -> ~95 B/entry) with zero behavior change. Ship this alone first.

2. STEP 2 - Slot stability. Add `flags::DEAD = 1<<3`, `live_count`, `free_slots: Vec<u32>`, `dead_bytes`. Delete `entries.swap_remove` from `remove_entry` (index.rs:196) and tombstone instead; `add_entry` pops the freelist. `len()`/`is_empty()` return `live_count`. Introduce the two private choke points `register_slot`/`unregister_slot` that ALL mutation routes through, including `add_entry`'s existing-FRN update path (index.rs:166-173) which behaves as a rename. Audit every `entries.iter().enumerate()` site (matching.rs:86, :94, :121) to skip DEAD; add a `live_entries()` iterator. GATE: `apply_create_delete_fixes_frn_map` (update only its stale swap_remove comment, not its assertions), `rebuild_after_delete_keeps_matches_consistent`, `apply_create_existing_frn_updates`. NEW TEST: delete then create so a slot is recycled, then query both the recycled entry and every surviving entry. Prerequisite for steps 3-8.

3. STEP 3 - NUL-delimited folded arena + query sanitization. Arena becomes `0x00 rec 0x00 rec 0x00`; `intern` maintains the leading and trailing delimiters and drops names whose folded form contains a byte < 0x20 (alongside the existing empty-name drop). Delete and rename NUL-fill the old folded record. Classify tiers from `arena[h-1]`/`arena[h+qlen]` instead of comparing against `e.folded_off`. Reject or strip control bytes from the folded query at the top of `search()` — queries arrive from the untrusted pipe. GATE: `no_cross_entry_arena_matches`, `rebuild_after_delete_keeps_matches_consistent`, `cjk_substring`, `astral_plane_utf16_ranges`. NEW TESTS: (a) index "xa" then "aab", query "aa" must now return frn 2 at 0.9 — this is SPEC DECISION 8, the latent straddle bug, and it FAILS on today's code; (b) a query containing U+0000 returns empty and never matches across a delimiter; (c) a name whose folded form contains a control byte is dropped.

4. STEP 4 - THE LATENCY STEP. No structural index change; this is where the measured failures should already fall. Add `rank_key: Vec<u8>` (depth.min(127) | penalized<<7) and the static `DEPTH_PEN: [f32; 256]`; add the memoized cycle-guarded depth sweep to `finalize()`; add the writer-side debounced sliced depth-repair for directory reparents. Add `tier_upper_bound()` (Fuzzy = 0.5, NOT `tier_base`'s 0.3) and replace `best: HashMap<u32,Cand>` / `Cand` / `upgrade` / the whole-map loop at matching.rs:585-604 with the 2K-capacity min-heap + `seen` bitset + drain-dedup + consecutive-slot grouping + STRICT `>` pass-skip and hit-reject. `ix.depth_of(eidx)` leaves the hot path. GATE: `tier_ordering`, `dedup_across_tiers_keeps_best`, `max_results_caps_and_orders`, `hidden_ranks_below_normal`, `depth_penalty_prefers_shallow`, `cancellation_returns_partial`. NEW TESTS: (a) the DIFFERENTIAL TEST — run the new selector and an exhaustive reference scorer over thousands of randomized corpora and assert identical result vectors, with corpora deliberately seeded to produce equal scores at the K boundary; (b) cached depth equals `depth_of` for every entry after `finalize()`, including a child-before-parent insertion order. BENCH GATE: initials-2, substring and common-substr p95 must be below ~4 ms at 300k. If they are not, the cost model is wrong — stop and escalate to Step 12 before doing structural work.

5. STEP 5 - `arena_recs` + `owner` replace `folded_order`. Append-only `arena_recs: Vec<ArenaRec>` (sorted by construction; never sorted at runtime) plus the 64-byte-block `owner` index into it. O(1) hit->slot; delete `entry_at` and its `partition_point`, and delete the `folded_order` build + `sort_unstable` half of `ensure_basic`. GATE: `no_cross_entry_arena_matches`, `apply_rename_updates_name_and_parent`. NEW TESTS: (a) RENAME TO A SAME-LENGTH NAME — the old name must no longer match; this is what the `rec.off == entries[slot].folded_off` liveness compare exists for and it fails without it; (b) invariant test: `arena_recs` strictly ascending in `off`, and `owner[b]` indexes the record containing byte `b*64`.

6. STEP 6 - Fixed-stride initials column. `initials: Vec<u8>` at stride 8; rewrite `segment_spans` (matching.rs:149) as an allocation-free `for_each_segment_start`, keeping the allocating wrapper for tests and `initials_ranges`. Mapping becomes `hit >> 3` with `(hit & 7) + qlen <= 8`; skip Pass B when `fq.len() > 8` (this is what keeps the `exact` class from regressing on the wider array). Zero the lane on delete. Delete `initials_arena`, `initials_spans`, `InitialsSpan`, `initials_entry_at`, and the remainder of `ensure_basic`. GATE: `initials_camel_case`, `segment_spans_camel_and_separators`, `tier_ordering`, `fold_with_map_matches_fold_on_nfc_input`. NEW TEST: a 9-segment name with a 9-char initials query, documenting the truncation (SPEC DECISION 2).

7. STEP 7 - Replace trigram postings. Add `charclass_bsi` (64 slices, 4 KiB growth chunks, bits CLEARED on delete — not optional, since slots are recycled and a stale union degrades selectivity monotonically), `tri_present` (2^24 bits, exact, per-record so no trigram spans a delimiter), `bi_present`, `uni_present`. Wire the arena presence sets to skip Pass A ONLY — never Pass C, since fuzzy does not require contiguity, and never Pass B, which asks a contiguity question about a different column and so needs sets of its own (issue #8). Add the depth-bucketed shallowest-first fuzzy drain with `d_max` derived from the floor. Rewrite `fuzzy_density` (matching.rs:310) allocation-free with a stack `[char; 64]` query buffer — it currently collects two `Vec<char>` per candidate, i.e. 40,000 allocations per query at FUZZY_CAP. Delete `ensure_trigrams` and the `trigrams` field. GATE: `fuzzy_subsequence_density_and_ranges`, `fuzzy_density_basics`, `fuzzy_skipped_below_three_bytes`; bench's `every_query_class_hits_the_corpus` must still find the planted family AND still return empty for `NO_MATCH_QUERIES`. NEW TESTS: (a) the gapped subsequence "a_b_c_d" vs query "abcd" now matches (SPEC DECISION 1 — this FAILS on today's code); (b) a `tri_present` miss returns empty without scanning (assert via a cfg(test) scan counter); (c) a `tri_present` miss does NOT suppress a genuine fuzzy hit.

8. STEP 8 - Delete the accel mutex; close issue #3. Move the former `Accel` fields into `VolumeIndex` as plain fields; delete `Accel`, `dirty`, `accel_lock` (index.rs:120), `mark_dirty` (index.rs:124), `Mutex<Accel>`; `search(&self)` and `ram_bytes(&self)` become lock-free. Batch USN application to one write-lock acquisition per journal read batch. GATE: confirm `VolumeIndex: Sync`. NEW TESTS: N searcher threads plus one writer thread applying Create/Delete/Rename — no panic, every returned FRN resolves through `path_of`, and results are always a valid snapshot. NEW BENCH CLASS: concurrent search with 4 threads on one index, asserting near-linear scaling — this is the regression tripwire that keeps issue #3 closed.

9. STEP 9 - In-place compaction. One ascending pass over `arena_recs` drives `copy_within` leftward compaction of BOTH arenas (live records are ascending in `arena_recs` order in both, so write cursor <= read cursor and the transient is zero); then truncate + `shrink_to_fit`, renumber slots, rebuild `owner`/columns/presence sets in place, rewrite the FRN table's VALUES through a `remap: Vec<u32>` (keys unchanged, so hash positions unchanged — never rebuild the table), re-run the depth sweep, drain `free_slots`. Wire triggers (dead_bytes > 25%, stale recs > 25%, dead slots > 12.5%, 15-min snapshot cadence) with machine-idle gating per SPEC 3.6 and a hard 40% threshold that forces it outside idle. Wire `VS_REBUILDING` + `index_epoch` in state.rs. Put the NEVER-BUILD-AND-SWAP rule and its arithmetic in a comment on `compact()`. GATE: NEW TEST — churn N creates/deletes, force compaction, assert search results are byte-identical before and after and `ram_bytes` drops; assert with a counting allocator that peak allocation never exceeds trigger + 6 MB/1M.

10. STEP 10 - Randomized property test as the standing safety net. Apply random Create/Delete/Rename sequences interleaved with searches and compactions, diffing `search()` against a naive linear matcher over the live set, and assert the invariants: `arena_recs` monotone in `off`; every column length == `entries.len()`; `live_count == frn_map.len()`; `dead_bytes` accurate; every live record's bytes are non-NUL and every dead record's are NUL. This is the net for the silent slot/column-desync failure mode, which produces wrong results rather than a crash.

11. STEP 11 - Service path (session.rs). Return `(frn, score, tier, slot)` from search and expand `match_ranges` only for items surviving the ext/path filters; push the ext filter into `flush` as a name-suffix predicate so `fetch == max` again instead of `max*8` clamped to 4096 (session.rs:269-273). GATE: new indexd test for filtered-search correctness; new bench class WITH filters set, which the harness has never exercised.

12. STEP 12 - MEASURED-GATED, optional, -27.3 B/entry. Open-addressed `frn_index: Vec<u32>` (2^21 slots, key re-derived from `entries[slot].frn`, multiply-shift hashing since NTFS packs a sequence number into the FRN's high bits so identity hashing is wrong) replacing `HashMap<u64,u32>`. Takes the budget from 147.46 to 120.20 MB/1M. Stage LAST and drop it if the schedule tightens — the cap closes without it. Move it earlier only if Step 0's real-volume L measurement comes back above 30 B.

13. STEP 13 - MEASURED-GATED, optional, only if Step 0's 1M scan probe or Step 4's 300k gate says the memmem scan itself eats the budget. In order of preference: (a) chunk the arena scan across 2-4 std threads with `qlen-1` overlap and merge per-thread 2K heaps — embarrassingly parallel, no new dependency, but burning cores per query is a policy question under SPEC 2.3's multi-session model; (b) a 2-byte prefix/exact CSR index (`p2_start: [u32; 65537]` + `p2_flat: Vec<u32>` rebuilt at compaction, plus a bounded post-compaction delta map), +4.3 MB/1M, which answers Exact+Prefix scan-free and lets the pass-skip fire BEFORE Pass A runs — the direct fix for initials-2 at 1M.

## Behavior changes requiring sign-off

- 1. FUZZY PREFILTER WIDENS - MORE RESULTS. Candidate generation for the fuzzy tier changes from 'names containing all query trigrams' to 'names whose character-class set is a superset of the query's'. The class mask is a strict superset of the trigram set (all trigrams present implies all characters present), so every fuzzy hit returned today is still returned, PLUS genuine subsequences that trigram intersection silently drops (query 'abcd' vs name 'a_b_c_d' has neither 'abc' nor 'bcd' contiguous). All survivors are still verified exactly by `fuzzy_density`, so no false positives. Evidence this is a bug and not a feature: bench.rs must synthesize `plant_fuzzy_name` corpus members because otherwise 'the fuzzy class degenerates into trigram lookup misses'. SPEC 3.4's sentence 'candidates = names containing all query trigrams' MUST be amended. Impact is bounded - fuzzy base <= 0.5, so new rows only surface on thin result pages.

- 2. INITIALS TIER TRUNCATED TO AN 8-BYTE LANE, and queries longer than 8 bytes skip the tier entirely. A name with more segments than fit in 8 UTF-8 bytes (roughly >8 ASCII segments, fewer with accented or CJK initials) loses its trailing initials. Real names carry 2-4 segments and bench's initials queries are 2-3 chars. SPEC 3.4 already states CJK and unsegmented scripts match via the substring tier, so the CJK narrowing is spec-sanctioned. The lane width is one constant; 12 B/entry buys 12 initials if the ruling goes the other way (+4 MB/1M, budget has room).

- 3. RANKING DEPTH CLAMPED AT 127. `rank_key` packs depth in 7 bits. The public `depth_of()` and `path_of()` keep PATH_DEPTH_CAP = 512 semantics untouched, so `path_of_cycle_is_finite` and `depth_of_counts_parents_walked` pass as written. The penalty difference between depth 127 (1/3.54) and 512 (1/11.2) is only reachable on a corrupt or cyclic parent chain.

- 4. BOUNDED DEPTH STALENESS AFTER A DIRECTORY REPARENT. Descendants of a moved directory carry a depth stale by a constant offset until the writer-side repair sweep lands (debounced ~500 ms, sliced at 64 Ki entries). Affects `depth_penalty` only, which SPEC 3.4 defines as a mild preference: it can transiently reorder two rows whose scores differ only by depth, and NEVER changes which entries match. Explicitly NOT done on the query path, unlike one of the candidate designs - putting it there would re-arm the very cliff this work removes.

- 5. `len()` RETURNS A MAINTAINED LIVE COUNT, not `entries.len()`, because deletes tombstone instead of swap-removing. External semantics are unchanged (`apply_create_delete_fixes_frn_map` passes as written; only its stale swap_remove comment needs updating). Internal code that treated `entries.len()` as the live count must be audited - this is an internal invariant change with a wrong-results failure mode if a site is missed.

- 6. TIE-BREAK ORDER AMONG EXACTLY-EQUAL SCORES CHANGES AFTER CHURN. `Scored::cmp` breaks equal scores by lower `eidx`. Entry indices now come from a freelist rather than being densely compacted by `swap_remove`, so after deletes and creates the index of an entry - and hence which of two exactly-equal-scoring rows sorts first - differs from today. Still fully deterministic for a given event history; only the ordering of exact ties changes. None of the 81 tests exercises this, so it will not be caught automatically.

- 7. QUERIES CONTAINING CONTROL BYTES (< 0x20) ARE REJECTED OR STRIPPED, and names whose folded form contains a control byte are dropped at `intern` alongside the existing empty-name drop. Required by the NUL-delimited arena, and correct hygiene regardless: queries arrive from the untrusted pipe (SPEC 3, SPEC 8.1). NTFS forbids these bytes in names, so real volumes are unaffected; only synthetic/walk-mode inputs can hit it.

- 8. LATENT BUG FIXED - RESULTS GAINED. Today's Pass 1 uses `memmem::find_iter`, which yields NON-OVERLAPPING matches; when a match straddles two arena records the bounds check at matching.rs:286 rejects it and the finder resumes at hit+len, skipping a real match that started inside the straddle. Traced against the shipped code: index `"xa"` then `"aab"`, query `"aa"` - the arena is `"xaaab"`, the only hit is at offset 1, `entry_at` rejects it, the finder resumes at 3, and entry 2 (literally named `aab`, a genuine 0.9 PREFIX match) is never returned. NUL delimiters make cross-record matches impossible, so no hit is ever rejected and no resume point can skip a real match. This ADDS results in rare adjacency cases.

- 9. FUZZY_CAP TRUNCATION RULE CHANGES. When the cap binds, the retained set is now depth-prioritized (shallowest first) rather than arena-order. Defensible because fuzzy base <= 0.5 regardless of density, so `score = base * depth_penalty` means shallow candidates dominate and the dropped ones are exactly those least able to reach the page. Treat the cap VALUE (20,000 -> possibly 4,000) as a SEPARATE, measured decision - do not change it in the same commit as the rule.

- 10. PROPOSED BUT NOT ADOPTED BY DEFAULT - FUZZY_MIN_DENSITY floor. One candidate design proposed rejecting fuzzy matches whose span exceeds ~5x the query length, as a counterweight to the widened prefilter. It IS a quality argument (a 6-char query scattered across a 200-char name is not a useful hit) and it would restore a structural guarantee that bench's NO_MATCH queries return empty rather than merely overwhelmingly probably empty. But it REMOVES results and needs its own ruling and its own measurement on a real corpus. Default position: do not adopt; revisit only if the widened prefilter produces visible junk.

- 11. NOT A BEHAVIOR CHANGE, BUT MUST BE ARGUED IN REVIEW BECAUSE IT IS THE MAIN LATENCY MECHANISM. Pass skipping and hit rejection are result-preserving. `tier_upper_bound(t)` is an exact upper bound on any score tier t can produce (depth_penalty <= 1, hidden_penalty <= 1). Both rules use STRICT `>` / `<`, which is REQUIRED: `Scored::cmp` (matching.rs:465-469) is `score.total_cmp(...).then_with(|| other.eidx.cmp(&self.eidx))`, so on equal score a LOWER eidx compares Greater and legitimately displaces the current K-th; a `>=` rule would silently discard those tie-break winners. The 2K-capacity heap plus drain-dedup is exactly sufficient (proof: each of the K-1 better distinct slots contributes at most 2 offers, so the K-th best distinct slot's best offer ranks at worst 2K-1). The fuzzy bound MUST be hard-coded as 0.5, never `tier_base(Fuzzy)` = 0.3, which is documented in the source as a floor. The differential test in Step 4 is the proof obligation; the existing tests cannot catch a violation.

- 12. NEW DEGRADATION MODE, LOGGED AND COUNTED. The design intentionally has NO per-tier candidate cap (unlike one candidate design), because truncating a tier drops an arbitrary subset. The only bounded-work valves are the existing FUZZY_CAP (now depth-prioritized) and `is_cancelled`, whose partial-results semantics SPEC 3.4 already permits. If a hard `CANDIDATE_BUDGET` safety valve is later added for pathological inputs, it must be logged, counted, and signed off separately - it is not part of this plan.

### F. Re-derivation at measured L (issue #10, M1)

Three real-corpus measurements of the mean name length, all `ram_breakdown()`
on the index as built:

| corpus | entries | L (name arena B/entry) | total B/entry | source |
|---|---:|---:|---:|---|
| `C:\` walk, unelevated, user-file-heavy subtrees | 554,032 | **34.6** | 165.38 | issue #10 |
| `C:\` walk, unelevated, same machine, fuller walk (head column present) | 755,774 | **30.0** | 148.66 | `yspot-indexd` startup log, 2026-09-03 |
| `C:\` MFT enumeration incl. a 250k short-name synthetic corpus | 1,093,055 | ≈ 25 (backed out) | 143.0 | `docs/M0.md` sign-off |

So L is **volume-dependent in the 25–35 B range**, and the synthetic bench
corpus (L ≈ 19) sits below every real point. The model's L = 23 was at the
bottom of the real range, not the middle.

**Validation of the model at L = 30.** The 755k walk measured 148.66 B/entry
with `frn_map` at 23.59 (hashbrown holding 2^20 buckets, which it keeps up to
~917k live entries). Restated at the 1M row count the model assumes — map at
2^21 buckets (35.65) — that is **160.7 B/entry**, against the model's 163.9 at
L = 30 with the head column (below). The model is within 2% once the map's
power-of-two cliff is accounted for; its only systematic overshoot is the
4.19 B/entry allocation slack, which `finalize()` hands back.

**The head column (issue #11)** adds row 7b: `head: Vec<u8>`, stride 2,
**2.00 B/entry** steady, ×1.125 at the trigger. It is in every total below.

Rows 2, 3 and 6 scale with L at 2.0625 B/entry per byte of L (name, folded
+ NUL, and 4 B of `owner` per 64 B of folded arena); at the compaction
trigger the arenas carry 25% stale bytes, so the slope there is 2.578.

| L | Phase A steady | Phase A at trigger | **Phase A peak** | Phase B steady | Phase B at trigger | Phase B peak |
|---:|---:|---:|---:|---:|---:|---:|
| 23 (plan) | 149.5 | 170.5 | 175.0 (87.5%) | 122.2 | 143.2 | 147.7 |
| 25 (MFT run) | 153.6 | 175.6 | 180.1 (90.1%) | 126.3 | 148.4 | 152.9 |
| **30 (755k walk)** | **163.9** | **188.5** | **193.0 (96.5%)** | 136.6 | 161.3 | 165.8 |
| **34.6 (554k walk)** | **173.4** | **200.4** | **204.9 (102.4%)** | 146.1 | 173.1 | 177.6 |

(Peak = trigger + the 4.50 MB `remap` transient; percentages are of the
200 MB/1M hard cap. Phase B = Phase A − 27.26 for the open-addressed
`frn_index` of Step 12.)

### §G. The same table, measured (2026-09-03)

§F is arithmetic. `crates/yspot-index/tests/compaction_peak.rs` measures it
with a counting global allocator — the gate Step 9 asked for and never got —
building a corpus at a chosen mean folded-name length, churning until
`should_compact()` fires, and recording the allocation high-water mark across
`compact()` itself. At 1M entries, which is the size the per-1M budget is
written for:

| L | steady | at trigger | **peak** | transient |
|---:|---:|---:|---:|---:|
| 26.0 | 142.6 | 163.2 | **167.9 (84.0%)** | 4.77 |
| 31.0 | 152.4 | 176.1 | **180.8 (90.4%)** | 4.77 |
| 36.0 | 162.3 | 190.0 | **194.8 (97.4%)** | 4.77 |

**Three corrections to §F.**

1. **The transient was not 4.50 MB/1M; it was 12.4.** §F counts only the
   `remap` array. Pass 3 also built `fresh: Vec<ArenaRec>` with
   `Vec::with_capacity(new_len)` — 8 B per live entry, a second copy of
   `arena_recs`, which is the very thing this step's own rule forbids and its
   prose claims it does not do. `arena_recs` is now rewritten in place like
   every other column, on the same write-cursor-never-passes-read-cursor
   argument that already justifies the arena copies, and the measured
   transient is **4.77 MB/1M** — the `remap` array and nothing else, as
   written. The test is the standing tripwire.
2. **§F over-states steady state by about 11 MB/1M**, and the peak with it. Its
   173.4 at L = 34.6 measures 162.3 at L = 36 — a *higher* L. The two errors
   ran in opposite directions and roughly cancelled, which is why §F's totals
   looked plausible.
3. **The cap is not breached anywhere in the measured range.** §F's headline
   — "102.4% at L = 34.6", the whole argument for scheduling Step 12 — does
   not reproduce: the measurement is **97.4% at L = 36**, past the worst real
   corpus (34.6, the 554k walk).

**What this does and does not settle.** The corpus is synthetic — generated
names of a controlled mean length under one parent — and the first version of
this note called that a caveat. It mostly is not, and the test now says so:
the arenas store names end to end, so their size is the *sum* of the lengths
and nothing else, and every other per-entry structure is a fixed stride or a
fixed-size set. Two corpora with the same mean and deliberately opposite
shapes (clustered, versus seven short names to one very long) measure
**identical to three decimal places** in both steady state and peak. Only the
mean counts, which is the one parameter the table is indexed by.

What does remain: the number measured is allocator bytes, while §10's gate is
RSS. M0's 1.09M run reported 163 MB RSS against a model of ~155, so roughly
5% slack sits on top of every figure above. Applying it, L = 36 lands at
~205 MB — marginally over — while L ≤ 31 stays clear. That is the real
remaining uncertainty, and it is a question about allocator behaviour rather
than about the corpus.

So the honest reading is that Phase A holds across the measured range with
the margin thinning at the top of it, not that it fails at L = 34.6. Step 12
buys 27 B/entry of headroom; it is no longer required to clear the cap.

**What changes.**

1. **Steady state holds everywhere measured.** Phase A steady is 164–173
   B/entry across the real range — 82–87% of the cap — and the measured
   1.09M RSS of 163 MB (`docs/M0.md`) agrees. The head column's 2 B/entry
   fit inside that; it was admitted on this table.
2. **The compaction transient no longer clears the cap on name-heavy
   volumes.** Phase A's peak is 96.5% of the cap at L = 30 and **102.4% at
   L = 34.6**. The plan's "Phase A stays under the cap up to L ≈ 34" (§D)
   was computed without the head column and at the edge; with it, the
   crossing is at **L ≈ 32.6**. Tightening the trigger (20% stale / 20%
   recs / 10% dead instead of 25/25/12.5) buys back only ~5 B/entry — 199.3
   at L = 34.6, at the line — so the thresholds are not the lever.
3. **Step 12 is the lever, and is no longer optional by the plan's own
   criterion.** The open-addressed `frn_index` (−27.26 B/entry) puts Phase B's
   peak at 166–178 B/entry (83–89%) across the whole measured range, with
   the 4 B/entry chunk slack still on top. The plan staged it last and
   marked it optional because "the cap closes without it" at L = 23; at the
   measured L it is what closes the cap during compaction on a
   user-file-heavy volume. **Recommendation: schedule Step 12 in M1, before
   the first reference-volume MFT measurement of L would be needed to
   decide it** — it is the same ~120 lines either way, and the remaining
   headroom (Phase B leaves 22–34 B/entry at peak) is what any later
   per-entry spend has to come out of.
4. **Every "the budget has room" argument in §A–D is to be read at L = 30–35,
   not 23.** In particular the +8 B/entry second BSI slice the fuzzy tier
   once contemplated (§Open questions) would push Phase A's steady state to
   181 at L = 34.6 and its peak to 213 — it is only affordable after Step 12.
   (Issue #7 was resolved without it.)

`yspot-indexd` now logs this breakdown, in B/entry with L, after every
enumeration, so the reference-machine runs (`docs/M0.md`, Machine B) record
L alongside the total.

## Open questions

- What is the REAL average folded-name length on the 1.33M-entry volume? Everything in the byte accounting scales through this one parameter (rows 2, 3, 6 at ~2.06 B/entry per byte of L). I derived L = 23 B from the measured 188,743,680 B cold figure by backing out `entries` capacity 2^21 and `frn_map.capacity()*16`, then dividing the residual arena CAPACITY by the ~1.5x mean doubling slack - the two judges guessed 17 and 26 with no derivation. Phase A holds to L ~= 34 and Phase B to L ~= 63, so this is a re-plan trigger, not a redesign trigger, but it decides whether Step 12 stays optional. Measure it in Step 0 by reporting `name_arena.len()` (not capacity) / `live_count`.
- Is `initials` stride 8 or stride 12? Stride 8 is 8 MB/1M and truncates names with >8 segments; stride 12 is 12 MB/1M and covers essentially everything. The budget has room for 12 in both phases. This is a product ruling on how much the camel/initials tier is worth, and it should be made against real-corpus segment-count distribution, not synthetic.
- How aggressive should compaction be? The plan gates it on machine-idle (SPEC 3.6, every session reporting idle) with a hard 40% forcing threshold, and accepts a ~150-350 ms write-lock stall at 1M as the deliberate price of never needing 2x resident memory. Alternatives: check an in-flight-search counter and defer; or accept a higher steady-state garbage fraction. Needs a call, and it interacts with the SPEC 3.7 15-minute snapshot cycle (they should share machinery rather than being independent subsystems).
- Does the fuzzy tier need a second BSI word over character PAIRS (+8 B/entry)? The single character-presence word is order-insensitive, so a query of common letters against long names could pass far more entries than the ~0.18^k estimate. FUZZY_CAP bounds the verification cost, but the cap binding more often than today means visible missing results rather than latency. Cannot be answered from the synthetic corpus - needs survivor counts measured on the real 1.33M volume before committing.
- Should the arena scan be parallelized (Step 13a), and if so under what policy? It is the cleanest fix for initials-2 at 1M (4 ms -> ~1.2 ms on 4 cores, no new dependency), but SPEC 2.3 says one machine-wide index serves every session, so burning 4 cores per query is a fairness question that the current thread-per-query shape in session.rs makes worse. Needs an architectural ruling alongside the worker-pool question below.
- session.rs spawns a fresh OS thread per query (~50-100 us of the 10 ms budget). Removing the accel mutex is what finally makes that thread able to run in parallel, so the spawn cost becomes the next visible item. Replacing it with a small worker pool is a follow-up outside this plan - but it should be scoped now, because it interacts with any decision to parallelize the scan.
- How should the bench harness's four `lazy_rebuild_us` rows (bench.rs:1289-1310) be replaced? After Step 4 they read the same as the steady-state class means and stop being a tripwire. Proposed replacement: direct per-event `apply()` timing; 'first query after N USN events' rather than one; a sustained-mutation class with a writer thread applying events WHILE the query classes run (the current probe only measures the first query after one event); and the concurrent-search class from Step 8. Needs agreement before Step 4 lands, or the regression tripwire silently stops tripping.
- No new dependencies are proposed - everything uses std, `memchr` (already a dependency), and `unicode-normalization`. If the hand-rolled open-addressed FRN table in Step 12 is judged not worth ~120 lines of tombstone/resize/load-factor code, the alternative is `hashbrown` with a custom `RawTable`, which would need a cleanroom and licence check (and note SPEC 3.9 already establishes that this project takes licence provenance seriously). The plan deliberately does not depend on it - the cap closes at 147.46 MB/1M without it.
- The plan assumes `FSCTL_ENUM_USN_DATA` order requires the `finalize()` depth sweep but that steady-state USN Creates virtually always arrive after their parent. If real journal traffic shows a meaningful rate of child-before-parent Creates, those entries get depth 0 until the next repair sweep and rank too high. Worth measuring the out-of-order rate during Step 0; if non-trivial, the debounced repair sweep needs a lower debounce or an out-of-order counter as its trigger.
