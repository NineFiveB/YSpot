//! `bench` — latency benchmark harness for the filename index (SPEC §10 M0).
//!
//! # What this measures — and what it deliberately does not
//!
//! It exercises the *matching path only* (§3.4): synthesize a filename corpus,
//! push it through [`EntrySink`], then time [`VolumeIndex::search`] per query
//! class against the §2.5 budget line "service first batch ≤ 10 ms (p95)".
//!
//! It is **not** M0 sign-off, and must never be quoted as such:
//!
//! * **The corpus is synthetic.** A seeded generator produces plausible name
//!   shapes, but a real NTFS volume has different name-length, extension and
//!   depth distributions and far more duplicate/near-duplicate structure. Both
//!   candidate volume and `memmem` behaviour depend on exactly that.
//! * **No MFT/USN I/O happens here.** §3.2's "15 s per 1M files" budget is
//!   untouched; `build_ms` below is arena + hashmap insert cost only.
//! * **No pipe, no serialization, no §3.8 security trimming.** §2.5 allots
//!   2 ms of the 10 ms to AccessCheck trimming, which this harness never
//!   spends. Read every number here as a *lower bound* on the real service.
//! * **CI runners are shared, throttled VMs.** §10 M0 requires numbers from
//!   reference Machine A (NVMe/8-core) and Machine B (i5-5200U class). CI
//!   results are a regression tripwire, not the sign-off measurement.
//!
//! Alongside the query classes it reports three things the classes cannot
//! show on their own: a per-structure `ram_bytes` table (so §4.3's
//! `ram_bytes.filename` is attributable rather than one opaque number), the
//! measured mean name length `L` that the §3.4 byte accounting scales
//! through, and an isolated Pass-A scan probe — one `memmem` over the folded
//! arena with a needle that cannot hit, which is the irreducible floor under
//! every query.
//!
//! Determinism: a fixed-seed inline xorshift64\* PRNG (no `rand` dependency);
//! nothing wall-clock or environment derived feeds corpus generation, so two
//! runs on one machine differ only by timing noise.
//!
//! # Usage
//!
//! ```text
//! bench [--entries N] [--iterations N] [--seed N] [--max-results N]
//!       [--json] [--json-out PATH] [--strict]
//! ```
//!
//! `--entries 1000000` is the §10 M0 target size (underscores accepted).
//!
//! `--json` writes one JSON object to stdout and nothing else. `--json-out`
//! writes the same object to a file while keeping the human table on stdout
//! (that is the CI shape: one run, readable log plus an archivable artifact).
//! `--strict` turns a p95 budget miss into exit code 1; without it the harness
//! is advisory and always exits 0 on a completed run.

use std::fmt::Write as _;
use std::time::Instant;

use yspot_index::index::{RamBreakdown, VolumeIndex};
use yspot_index::{flags, EntrySink, UsnEvent};

/// §2.5: service first batch (in-memory match + rank + trimming) ≤ 10 ms p95.
const BUDGET_P95_US: f64 = 10_000.0;
/// §3.4 memory target, per 1M entries: typical, then the hard cap.
const RAM_TYPICAL_PER_1M: f64 = 120.0 * 1024.0 * 1024.0;
const RAM_CAP_PER_1M: f64 = 200.0 * 1024.0 * 1024.0;
/// §2.5 / §4.3: the service's first batch is the top ~32 hits.
const DEFAULT_MAX_RESULTS: usize = 32;
/// Folded-arena size the §3.4 byte accounting puts a 1M-entry volume at, used
/// only to extrapolate the isolated scan probe onto that reference point.
const REFERENCE_ARENA_BYTES: f64 = 24.0 * 1024.0 * 1024.0;
/// Representative queries generated per class.
const QUERIES_PER_CLASS: usize = 6;

const USAGE: &str = "usage: bench [--entries N] [--iterations N] [--seed N] \
                     [--max-results N] [--json] [--json-out PATH] [--strict]";

// ---------------------------------------------------------------------------
// PRNG — inline xorshift64*, deterministic, no dependency.
// ---------------------------------------------------------------------------

/// xorshift64\*: 64 bits of state, one multiply on output. Good enough for
/// corpus shaping and — unlike anything seeded from the clock — reproducible.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // A zero state is a fixed point of xorshift; force it non-zero.
        Rng(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform-ish in `0..n`. The modulo bias is ~2^-64 for the small `n` used
    /// here and irrelevant to a corpus shape.
    fn below(&mut self, n: usize) -> usize {
        debug_assert!(n > 0);
        (self.next_u64() % n as u64) as usize
    }

    fn pick<T: Copy>(&mut self, s: &[T]) -> T {
        s[self.below(s.len())]
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
}

// ---------------------------------------------------------------------------
// Vocabulary.
// ---------------------------------------------------------------------------

/// Words used to build file names. Lower-case; casing is applied per style.
const FILE_WORDS: &[&str] = &[
    "config",
    "service",
    "data",
    "report",
    "test",
    "log",
    "index",
    "cache",
    "backup",
    "user",
    "project",
    "module",
    "client",
    "server",
    "session",
    "message",
    "request",
    "response",
    "buffer",
    "parser",
    "render",
    "monitor",
    "telemetry",
    "dashboard",
    "inventory",
    "scheduler",
    "migration",
    "analytics",
    "invoice",
    "summary",
    "draft",
    "final",
    "archive",
    "export",
    "import",
    "sample",
    "template",
    "snapshot",
    "profile",
    "account",
    "payment",
    "order",
    "product",
    "customer",
    "contract",
    "meeting",
    "notes",
    "agenda",
    "budget",
    "forecast",
    "roadmap",
    "design",
    "layout",
    "theme",
    "icon",
    "avatar",
    "banner",
    "thumbnail",
    "screenshot",
    "recording",
    "transcript",
    "manifest",
    "schema",
    "fixture",
    "loader",
    "helper",
    "common",
    "core",
    "main",
    "api",
    "auth",
    "token",
    "secret",
    "bundle",
    "chunk",
    "vendor",
    "legacy",
    "quarterly",
    "annual",
    "release",
    "handler",
    "adapter",
    "registry",
    "pipeline",
    "worker",
];

/// Directory names skew toward the real Windows/dev-tree vocabulary.
const DIR_WORDS: &[&str] = &[
    "src",
    "bin",
    "docs",
    "assets",
    "build",
    "target",
    "node_modules",
    "users",
    "program files",
    "windows",
    "system32",
    "temp",
    "downloads",
    "documents",
    "pictures",
    "music",
    "videos",
    "desktop",
    "appdata",
    "local",
    "roaming",
    "cache",
    "logs",
    "data",
    "test",
    "vendor",
    "lib",
    "include",
    "dist",
    "public",
    "archive",
    "backup",
    "projects",
    "workspace",
    "release",
    "debug",
    "packages",
    "resources",
    "templates",
    "reports",
];

/// Dot-directories, which real trees are full of.
const DOT_DIRS: &[&str] = &[
    ".git",
    ".vscode",
    ".cache",
    ".venv",
    ".idea",
    ".config",
    ".github",
    ".pytest_cache",
];

/// Accented Latin — exercises NFC normalization and the non-ASCII fold path.
const ACCENTED: &[&str] = &[
    "café",
    "résumé",
    "naïve",
    "Müller",
    "Ångström",
    "señor",
    "über",
    "crème",
    "Zürich",
    "Bjørn",
    "piñata",
    "Français",
    "Español",
    "Português",
    "jalapeño",
];

/// CJK — no segment structure, so these only ever reach the substring tier
/// (§3.4 says so explicitly). Included so the tier is actually stressed.
const CJK: &[&str] = &[
    "汉语手册",
    "项目文档",
    "设计说明",
    "年度报告",
    "数据备份",
    "日本語",
    "テスト資料",
    "会議メモ",
    "설계문서",
    "사용자설명서",
];

/// Extension mix, weighted by repetition (crude but explicit and readable).
const FILE_EXTS: &[&str] = &[
    "txt", "txt", "txt", "md", "md", "rs", "rs", "ts", "ts", "tsx", "js", "js", "json", "json",
    "json", "png", "png", "jpg", "jpg", "pdf", "pdf", "docx", "docx", "xlsx", "pptx", "dll", "dll",
    "dll", "exe", "exe", "log", "log", "log", "yaml", "toml", "csv", "zip", "html", "css", "h",
    "cpp", "py", "py", "xml", "xml", "ini", "dat", "tmp", "bak", "svg", "gif", "mp4", "mp3", "sql",
    "ps1", "bat", "lock", "cfg", "pdb", "obj",
];

const COMPOUND_EXTS: &[&str] = &[
    "tar.gz",
    "min.js",
    "d.ts",
    "spec.ts",
    "test.tsx",
    "backup.zip",
];

/// Stems for the planted fuzzy family; see [`plant_fuzzy_name`].
const FUZZY_STEMS: &[(&str, &str)] = &[
    ("telemetry", "agg"),
    ("dashboard", "sync"),
    ("inventory", "diff"),
    ("scheduler", "meta"),
    ("migration", "plan"),
    ("analytics", "raw"),
];

/// Nothing in the vocabulary produces these, so they exercise the "query
/// matched nothing" path (and, at ≥ 3 bytes, the trigram early-out).
const NO_MATCH_QUERIES: &[&str] = &["zqxjvw", "qzzxwv", "jjqxzv", "xkqvzz", "wvzqxj", "zzqjxv"];

/// Very common 4-byte substrings for the candidate-volume worst case. Every
/// one is the prefix of a high-frequency vocabulary word, so each matches tens
/// of thousands of entries at 1M — far past what the top-32 page needs.
const COMMON_SUBSTRINGS: &[&str] = &["conf", "serv", "data", "repo", "temp", "sess"];

/// Needle for the isolated Pass-A probe. Long, and built from letter runs no
/// vocabulary word contains, so it is guaranteed to match nothing: the scan
/// runs to the end of the folded arena and *only* the scan happens — no
/// hit→entry mapping, no tiering, no ranking. That makes the irreducible
/// `memmem` floor measurable on its own, which is the term SPEC §3.4's "a 1M
/// -name arena scans in single-digit ms" premise rests on.
const SCAN_PROBE_NEEDLE: &str = "zqxjvw-no-such-name-zqxjvw";

// ---------------------------------------------------------------------------
// Corpus synthesis.
// ---------------------------------------------------------------------------

/// Anchor for top-level entries. Matches the `walk`/`mft` convention: the
/// volume root gets no entry, so its children carry a parent FRN that is
/// absent from the index and resolve against `root_path` at depth 0.
const ROOT_FRN: u64 = 0;

/// Directory counts for levels 2..=7, relative weights (level 1 is fixed).
const DIR_LEVEL_WEIGHTS: [usize; 6] = [3, 12, 30, 30, 18, 7];
/// Parent-level distribution for files over levels 2..=7, i.e. file depths
/// 3..=8 — the "realistic depth distribution" §3.4 paths actually have.
const FILE_LEVEL_WEIGHTS: [usize; 6] = [5, 15, 30, 25, 17, 8];
const TOP_LEVEL_DIRS: usize = 24;

struct GenEntry {
    frn: u64,
    parent_frn: u64,
    name: String,
    flags: u16,
}

struct Corpus {
    entries: Vec<GenEntry>,
    dirs: usize,
    files: usize,
    /// Queries for the planted fuzzy family, in corpus order.
    fuzzy_queries: Vec<String>,
    gen_ms: f64,
}

/// NTFS packs a 16-bit sequence number into the high bits of an FRN, so real
/// FRNs are sparse rather than 0..n. Mimic that: a dense counter would give
/// the FRN hashmap an unrealistically friendly key distribution.
fn frn_of(i: usize) -> u64 {
    (((i as u64 % 64_000) + 1) << 48) | (i as u64 + 16)
}

fn push_lower(out: &mut String, w: &str) {
    out.extend(w.chars().flat_map(char::to_lowercase));
}

fn push_capitalized(out: &mut String, w: &str) {
    let mut cs = w.chars();
    if let Some(c) = cs.next() {
        out.extend(c.to_uppercase());
        push_lower(out, cs.as_str());
    }
}

fn word_count(rng: &mut Rng, is_dir: bool) -> usize {
    let r = rng.below(100);
    if is_dir {
        match r {
            0..=54 => 1,
            55..=87 => 2,
            _ => 3,
        }
    } else {
        match r {
            0..=29 => 1,
            30..=69 => 2,
            70..=91 => 3,
            _ => 4,
        }
    }
}

/// Version/date/counter suffixes, which real names carry constantly and which
/// lengthen the folded arena the substring scan walks.
fn push_suffix(rng: &mut Rng, out: &mut String) {
    let n = rng.below(4000);
    match rng.below(5) {
        0 => {
            let v = 1 + n % 9;
            let _ = write!(out, "-v{v}");
        }
        1 => {
            let y = 2015 + n % 11;
            let _ = write!(out, "_{y}");
        }
        2 => {
            let c = 1 + n % 20;
            let _ = write!(out, " ({c})");
        }
        3 => {
            let _ = write!(out, "{n}");
        }
        _ => {
            let _ = write!(out, "_{n}");
        }
    }
}

/// One synthetic name into `out`. `words` is a caller-owned scratch buffer so
/// generating a million names does not allocate a million Vecs.
fn gen_name(rng: &mut Rng, is_dir: bool, out: &mut String, words: &mut Vec<&'static str>) {
    out.clear();
    words.clear();

    if is_dir && rng.chance(8) {
        out.push_str(rng.pick(DOT_DIRS));
        return;
    }

    let pool = if is_dir { DIR_WORDS } else { FILE_WORDS };
    for _ in 0..word_count(rng, is_dir) {
        words.push(rng.pick(pool));
    }
    // ~5% non-ASCII overall, so the NFC + Unicode fold path is exercised on a
    // meaningful slice of the arena rather than as a token gesture.
    if rng.chance(3) {
        words[0] = rng.pick(ACCENTED);
    } else if rng.chance(2) {
        words[0] = rng.pick(CJK);
    }

    match rng.below(100) {
        // camelCase — the case the initials tier exists for.
        0..=14 => {
            for (i, w) in words.iter().enumerate() {
                if i == 0 {
                    push_lower(out, w);
                } else {
                    push_capitalized(out, w);
                }
            }
        }
        // PascalCase
        15..=27 => {
            for w in words.iter() {
                push_capitalized(out, w);
            }
        }
        // snake_case
        28..=47 => join(out, words, '_', false),
        // kebab-case
        48..=67 => join(out, words, '-', false),
        // Title Cased With Spaces
        68..=79 => join(out, words, ' ', true),
        // dotted.lower.case
        80..=87 => join(out, words, '.', false),
        // plainlowercase
        _ => {
            for w in words.iter() {
                push_lower(out, w);
            }
        }
    }

    if rng.chance(25) {
        push_suffix(rng, out);
    }
    if !is_dir {
        out.push('.');
        if rng.chance(5) {
            out.push_str(rng.pick(COMPOUND_EXTS));
        } else {
            out.push_str(rng.pick(FILE_EXTS));
        }
    }
}

fn join(out: &mut String, words: &[&str], sep: char, title: bool) {
    for (i, w) in words.iter().enumerate() {
        if i > 0 {
            out.push(sep);
        }
        if title {
            push_capitalized(out, w);
        } else {
            push_lower(out, w);
        }
    }
}

/// Build one member of the planted fuzzy family, returning `(name, query)`.
///
/// The fuzzy tier's prefilter (§3.4) keeps only entries whose folded name
/// contains **every** trigram of the query. Organic corpora almost never
/// satisfy that for a genuinely gapped subsequence, so without planting, the
/// fuzzy class degenerates into "trigram lookup misses, return nothing" and
/// measures the early-out instead of the tier. These names are built so the
/// query's trigrams all occur while the query itself does not occur
/// contiguously: `stem + "_" + last3(stem) + tail` against query `stem + tail`.
/// Example: `telemetry_tryagg.log` vs query `telemetryagg`.
fn plant_fuzzy_name(rng: &mut Rng, which: usize) -> (String, String) {
    let (stem, tail) = FUZZY_STEMS[which % FUZZY_STEMS.len()];
    let bridge = &stem[stem.len() - 3..];
    let ext = rng.pick(FILE_EXTS);
    (
        format!("{stem}_{bridge}{tail}.{ext}"),
        format!("{stem}{tail}"),
    )
}

fn weighted_level(rng: &mut Rng, weights: &[usize; 6]) -> usize {
    let sum: usize = weights.iter().sum();
    let mut r = rng.below(sum);
    for (k, &w) in weights.iter().enumerate() {
        if r < w {
            return k + 2;
        }
        r -= w;
    }
    7
}

/// Pick a parent FRN at `level`, walking shallower if that level came out
/// empty (only reachable for very small `--entries`).
fn parent_at(rng: &mut Rng, by_level: &[Vec<u64>], level: usize) -> u64 {
    for l in (1..=level).rev() {
        if !by_level[l].is_empty() {
            return rng.pick(&by_level[l]);
        }
    }
    ROOT_FRN
}

fn generate(entries: usize, seed: u64) -> Corpus {
    let t0 = Instant::now();
    let mut rng = Rng::new(seed);

    // §3.4-plausible mix: ~15% of MFT records on a real volume are directories.
    let dir_total = (entries * 15 / 100).max(TOP_LEVEL_DIRS + 6);
    let file_total = entries.saturating_sub(dir_total);

    let mut out: Vec<GenEntry> = Vec::with_capacity(entries);
    let mut by_level: Vec<Vec<u64>> = vec![Vec::new(); 8];
    let mut buf = String::new();
    let mut words: Vec<&'static str> = Vec::new();
    let mut next = 0usize;

    let l1 = TOP_LEVEL_DIRS.min(dir_total);
    for _ in 0..l1 {
        gen_name(&mut rng, true, &mut buf, &mut words);
        let frn = frn_of(next);
        next += 1;
        out.push(GenEntry {
            frn,
            parent_frn: ROOT_FRN,
            name: buf.clone(),
            flags: flags::DIR,
        });
        by_level[1].push(frn);
    }

    let remaining = dir_total - l1;
    let wsum: usize = DIR_LEVEL_WEIGHTS.iter().sum();
    let mut counts = [0usize; 6];
    let mut assigned = 0usize;
    for (k, &w) in DIR_LEVEL_WEIGHTS.iter().enumerate() {
        counts[k] = (remaining * w / wsum).max(1);
        assigned += counts[k];
    }
    // Remainder (or overshoot from the `.max(1)` floor) lands on level 5.
    counts[3] += remaining.saturating_sub(assigned);

    for (k, &count) in counts.iter().enumerate() {
        let level = k + 2;
        for _ in 0..count {
            gen_name(&mut rng, true, &mut buf, &mut words);
            let frn = frn_of(next);
            next += 1;
            let parent = parent_at(&mut rng, &by_level, level - 1);
            out.push(GenEntry {
                frn,
                parent_frn: parent,
                name: buf.clone(),
                flags: flags::DIR,
            });
            by_level[level].push(frn);
        }
    }

    // Planted fuzzy family: enough members to be findable, few enough not to
    // distort the corpus (≤ 0.05%).
    let plant_count = (file_total / 2000).clamp(FUZZY_STEMS.len(), 4096);
    let plant_stride = (file_total / plant_count.max(1)).max(1);
    let mut fuzzy_queries: Vec<String> = Vec::new();

    for i in 0..file_total {
        let level = weighted_level(&mut rng, &FILE_LEVEL_WEIGHTS);
        let parent = parent_at(&mut rng, &by_level, level);
        let name = if i % plant_stride == 0 {
            // Cycles through FUZZY_STEMS, so the query list stays at exactly
            // one entry per stem however many members get planted.
            let (n, q) = plant_fuzzy_name(&mut rng, i / plant_stride);
            if !fuzzy_queries.contains(&q) {
                fuzzy_queries.push(q);
            }
            n
        } else {
            gen_name(&mut rng, false, &mut buf, &mut words);
            buf.clone()
        };
        // Real volumes are full of hidden/system files; §3.4 penalizes them.
        let f = if rng.chance(6) { flags::HIDDEN } else { 0 };
        let frn = frn_of(next);
        next += 1;
        out.push(GenEntry {
            frn,
            parent_frn: parent,
            name,
            flags: f,
        });
    }

    let dirs = out.iter().filter(|e| e.flags & flags::DIR != 0).count();
    let files = out.len() - dirs;
    Corpus {
        entries: out,
        dirs,
        files,
        fuzzy_queries,
        gen_ms: ms(t0),
    }
}

// ---------------------------------------------------------------------------
// Query construction — every query is derived from the corpus so it hits.
// ---------------------------------------------------------------------------

fn is_sep(c: char) -> bool {
    matches!(c, '-' | '_' | '.' | ' ')
}

/// Mirror of `matching::segment_spans`, which is crate-private: maximal runs
/// of non-`-_. ` chars, split additionally at lower→upper camel transitions.
/// Only used to *construct* initials queries; if it ever drifts from the real
/// rule the symptom is a class reporting `mean_hits` 0, which the table flags.
fn segment_starts(name: &str) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut open = false;
    let mut prev: Option<char> = None;
    for (i, c) in name.char_indices() {
        if is_sep(c) {
            open = false;
        } else {
            let camel = matches!(prev, Some(p) if p.is_lowercase() && c.is_uppercase());
            if camel || !open {
                starts.push(i);
                open = true;
            }
        }
        prev = Some(c);
    }
    starts
}

fn initials_of(name: &str) -> String {
    let mut s = String::new();
    for st in segment_starts(name) {
        if let Some(c) = name[st..].chars().next() {
            s.extend(c.to_lowercase());
        }
    }
    s
}

fn take_chars(s: &str, n: usize) -> Option<&str> {
    let mut it = s.char_indices();
    for _ in 0..n {
        it.next()?;
    }
    let end = it.next().map(|(i, _)| i).unwrap_or(s.len());
    Some(&s[..end])
}

#[derive(Default)]
struct QuerySet {
    exact: Vec<String>,
    prefix: Vec<String>,
    word_boundary: Vec<String>,
    initials2: Vec<String>,
    initials3: Vec<String>,
    substring: Vec<String>,
}

fn push_unique(v: &mut Vec<String>, s: String) {
    if v.len() < QUERIES_PER_CLASS && !s.is_empty() && !v.contains(&s) {
        v.push(s);
    }
}

/// Walk the corpus on a stride and harvest one representative query per class
/// from real names, so every class actually reaches its tier.
///
/// Names are read back out of the index rather than off the corpus, so the
/// queries are built from exactly the NFC form the arenas hold — otherwise a
/// non-NFC generated name would yield a query that cannot match itself.
fn build_queries(entries: &[GenEntry], ix: &VolumeIndex) -> QuerySet {
    let mut q = QuerySet::default();
    let step = (entries.len() / 20_000).max(1);
    for e in entries.iter().step_by(step) {
        if e.flags & flags::DIR != 0 {
            continue;
        }
        let Some(name) = ix.name_of(e.frn) else {
            continue;
        };

        // Exact: whole name. Prefer longer names so the query is distinctive
        // rather than also being a prefix of half the corpus.
        if name.len() >= 14 && name.is_ascii() {
            push_unique(&mut q.exact, name.to_string());
        }
        // Prefix: the first six chars — roughly what a user has typed when the
        // first batch must already be on screen.
        if let Some(p) = take_chars(name, 6) {
            if p.len() < name.len() {
                push_unique(&mut q.prefix, p.to_string());
            }
        }
        // Word boundary: a segment that follows a separator.
        if let Some(sep) = name.find(is_sep) {
            let rest = &name[sep..];
            let seg: String = rest
                .chars()
                .skip(1)
                .take_while(|c| !is_sep(*c))
                .take(6)
                .collect();
            if seg.chars().count() >= 4 {
                push_unique(&mut q.word_boundary, seg);
            }
        }
        // Substring: interior of a segment, so the byte before the match is
        // not a separator and the match is not at offset 0.
        for st in segment_starts(name) {
            let seg: String = name[st..].chars().take_while(|c| !is_sep(*c)).collect();
            if seg.chars().count() >= 8 {
                let inner: String = seg.chars().skip(1).take(5).collect();
                push_unique(&mut q.substring, inner.to_lowercase());
                break;
            }
        }
        // Camel initials, the §10 M0 2-char and 3-char worst cases.
        let ini = initials_of(name);
        if ini.chars().count() >= 3
            && ini.is_ascii()
            && ini.chars().all(|c| c.is_ascii_alphabetic())
        {
            if let Some(p) = take_chars(&ini, 2) {
                push_unique(&mut q.initials2, p.to_string());
            }
            if let Some(p) = take_chars(&ini, 3) {
                push_unique(&mut q.initials3, p.to_string());
            }
        }

        if q.exact.len() == QUERIES_PER_CLASS
            && q.prefix.len() == QUERIES_PER_CLASS
            && q.word_boundary.len() == QUERIES_PER_CLASS
            && q.initials2.len() == QUERIES_PER_CLASS
            && q.initials3.len() == QUERIES_PER_CLASS
            && q.substring.len() == QUERIES_PER_CLASS
        {
            break;
        }
    }
    q
}

// ---------------------------------------------------------------------------
// Measurement.
// ---------------------------------------------------------------------------

fn ms(t0: Instant) -> f64 {
    t0.elapsed().as_secs_f64() * 1e3
}

fn us(t0: Instant) -> f64 {
    t0.elapsed().as_secs_f64() * 1e6
}

struct Stats {
    min: f64,
    p50: f64,
    p95: f64,
    /// Reported next to `max` because with a few hundred samples `max` is one
    /// sample and can be pure scheduler noise, while p99 still describes the
    /// tail the §2.5 budget is judged on.
    p99: f64,
    max: f64,
}

/// Nearest-rank percentiles over the raw sample set (no interpolation — with
/// a few hundred samples, interpolation would invent precision).
fn stats(mut v: Vec<f64>) -> Stats {
    if v.is_empty() {
        return Stats {
            min: 0.0,
            p50: 0.0,
            p95: 0.0,
            p99: 0.0,
            max: 0.0,
        };
    }
    v.sort_by(f64::total_cmp);
    let at = |p: f64| -> f64 {
        let rank = ((p * v.len() as f64).ceil() as usize).max(1);
        v[rank.min(v.len()) - 1]
    };
    Stats {
        min: v[0],
        p50: at(0.50),
        p95: at(0.95),
        p99: at(0.99),
        max: v[v.len() - 1],
    }
}

struct ClassResult {
    name: &'static str,
    note: &'static str,
    queries: Vec<String>,
    stats: Stats,
    mean_hits: f64,
    pass: bool,
}

/// Time `iterations` searches, cycling through `queries`, after a warm-up that
/// is discarded. Returns one sample per search.
fn measure_class(
    ix: &VolumeIndex,
    name: &'static str,
    note: &'static str,
    queries: Vec<String>,
    iterations: usize,
    max_results: usize,
) -> ClassResult {
    if queries.is_empty() {
        return ClassResult {
            name,
            note: "NO QUERIES GENERATED",
            queries,
            stats: stats(Vec::new()),
            mean_hits: 0.0,
            pass: false,
        };
    }

    let warmup = (iterations / 10).clamp(3, 25);
    for i in 0..warmup {
        let hits = ix.search(&queries[i % queries.len()], max_results, &|| false);
        std::hint::black_box(&hits);
    }

    let mut samples = Vec::with_capacity(iterations);
    let mut total_hits = 0usize;
    for i in 0..iterations {
        let q = &queries[i % queries.len()];
        let t0 = Instant::now();
        let hits = ix.search(q, max_results, &|| false);
        samples.push(us(t0));
        total_hits += std::hint::black_box(&hits).len();
    }

    let s = stats(samples);
    ClassResult {
        name,
        note,
        queries,
        pass: s.p95 <= BUDGET_P95_US,
        mean_hits: total_hits as f64 / iterations as f64,
        stats: s,
    }
}

// ---------------------------------------------------------------------------
// CLI.
// ---------------------------------------------------------------------------

struct Config {
    entries: usize,
    iterations: usize,
    seed: u64,
    max_results: usize,
    json: bool,
    json_out: Option<String>,
    strict: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            entries: 1_000_000,
            iterations: 200,
            seed: 0x59_53_50_4F_54_00_01,
            max_results: DEFAULT_MAX_RESULTS,
            json: false,
            json_out: None,
            strict: false,
        }
    }
}

fn parse_usize(flag: &str, v: Option<&String>) -> Result<usize, String> {
    let v = v.ok_or_else(|| format!("{flag} expects a value"))?;
    v.replace('_', "")
        .parse::<usize>()
        .map_err(|e| format!("{flag}: {e}"))
}

/// `Ok(None)` means `--help` was asked for.
fn parse_args(args: &[String]) -> Result<Option<Config>, String> {
    let mut cfg = Config::default();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "--help" | "-h" => return Ok(None),
            "--entries" => {
                cfg.entries = parse_usize(a, args.get(i + 1))?.max(1_000);
                i += 1;
            }
            "--iterations" => {
                cfg.iterations = parse_usize(a, args.get(i + 1))?.max(1);
                i += 1;
            }
            "--max-results" => {
                cfg.max_results = parse_usize(a, args.get(i + 1))?.max(1);
                i += 1;
            }
            "--seed" => {
                cfg.seed = parse_usize(a, args.get(i + 1))? as u64;
                i += 1;
            }
            "--json-out" => {
                cfg.json_out = Some(
                    args.get(i + 1)
                        .ok_or("--json-out expects a path")?
                        .to_string(),
                );
                i += 1;
            }
            "--json" => cfg.json = true,
            "--strict" => cfg.strict = true,
            other => return Err(format!("unknown argument {other:?}")),
        }
        i += 1;
    }
    Ok(Some(cfg))
}

// ---------------------------------------------------------------------------
// Report.
// ---------------------------------------------------------------------------

struct Report {
    entries_requested: usize,
    iterations: usize,
    seed: u64,
    max_results: usize,
    strict: bool,

    corpus_generated: usize,
    corpus_dirs: usize,
    corpus_files: usize,
    gen_ms: f64,

    inserted: usize,
    build_ms: f64,
    finalize_ms: f64,
    ram_cold: u64,
    ram_warm: u64,
    ram_cold_parts: RamBreakdown,
    ram_warm_parts: RamBreakdown,
    /// `name_arena.len()` / `folded_arena.len()` — payload, not capacity. The
    /// mean of these over `inserted` is the `L` the §3.4 byte accounting
    /// scales through, and it is the one parameter of that model this harness
    /// can actually measure.
    name_arena_bytes: usize,
    folded_arena_bytes: usize,
    ram_budget_typical: f64,
    ram_budget_cap: f64,
    ram_pass: bool,

    cold_basic_us: f64,
    cold_trigram_us: f64,
    mutation_basic_us: f64,
    mutation_trigram_us: f64,
    scan_probe_stats: Stats,
    scan_probe_hits: usize,
    path_of_stats: Stats,

    classes: Vec<ClassResult>,
    overall_pass: bool,
}

/// Printed with the memory block: the totals below moved for two independent
/// reasons and a run must not be read as a regression when it is a corrected
/// measurement.
const RAM_ACCOUNTING_NOTE: &str = "\
              NOTE: frn_map is charged per hashbrown BUCKET — 16 B payload plus one control byte,
              and capacity() reports only 7/8 of the buckets. That line is ~21% larger than the
              capacity()*16 it replaced, so totals RISE against pre-fix runs: a corrected
              accounting bug, not a memory regression. finish() then pulls them back down by
              handing back bulk-load doubling slack, which IS a real reduction.";

/// Printed with the isolated Pass-A probe; see [`SCAN_PROBE_NEEDLE`].
const SCAN_PROBE_NOTE: &str = "\
isolated Pass-A scan — one memmem over the folded arena with a needle that cannot hit, so the scan
runs end to end and nothing maps, tiers, scores or ranks. This is the floor every query pays before
doing any work of its own: above ~4 ms on a 1M-entry arena the scan alone eats the §2.5 budget and
no amount of per-candidate work can recover it.";

const HEADER_CAVEAT: &str = "\
Synthetic corpus, no MFT/USN I/O, no pipe, no §3.8 AccessCheck trimming (§2.5
allots 2 ms of the 10 ms budget to trimming that is NOT spent here). These are
LOWER BOUNDS on service latency. CI runners are shared, throttled VMs; SPEC §10
M0 sign-off requires numbers from reference Machine A and Machine B.";

/// The `RamBreakdown` fields in print/JSON order, so the table and the JSON
/// object cannot drift apart.
fn ram_rows(b: &RamBreakdown) -> [(&'static str, u64); 8] {
    [
        ("entries", b.entries),
        ("name_arena", b.name_arena),
        ("folded_arena", b.folded_arena),
        ("frn_map", b.frn_map),
        ("free_slots", b.free_slots),
        ("accel:folded_order", b.folded_order),
        ("accel:initials", b.initials),
        ("accel:trigrams", b.trigrams),
    ]
}

impl Report {
    /// Folded-arena bytes scanned per second, at the probe's p50.
    fn scan_gb_per_s(&self) -> f64 {
        let secs = self.scan_probe_stats.p50 / 1e6;
        if secs <= 0.0 {
            return 0.0;
        }
        self.folded_arena_bytes as f64 / secs / 1e9
    }

    /// The probe extrapolated onto the §3.4 reference point (a 1M-entry
    /// volume, 24 MB folded arena), so a run at any `--entries` can be read
    /// against the ~4 ms line that falsifies the "scan is not the bottleneck"
    /// premise (docs/design/accel-redesign.md, falsifier 2).
    fn scan_ms_at_1m(&self) -> f64 {
        if self.folded_arena_bytes == 0 {
            return 0.0;
        }
        self.scan_probe_stats.p50 / 1e3 * REFERENCE_ARENA_BYTES / self.folded_arena_bytes as f64
    }

    /// Per-structure attribution of `ram_bytes` (§4.3 `ram_bytes.filename`).
    /// Cold is the settled index; warm is after the lazy accel structures
    /// have been built, which is where the trigram postings appear.
    fn print_ram_table(&self) {
        println!(
            "ram_bytes by structure — cold = after finish(), warm = after the accel is built:"
        );
        println!(
            "{:<20} {:>14} {:>14} {:>10} {:>12}",
            "structure", "cold bytes", "warm bytes", "warm MB", "warm B/entry"
        );
        let n = self.inserted.max(1) as f64;
        for (&(name, cold), &(_, warm)) in ram_rows(&self.ram_cold_parts)
            .iter()
            .zip(ram_rows(&self.ram_warm_parts).iter())
        {
            println!(
                "{name:<20} {cold:>14} {warm:>14} {:>10.2} {:>12.2}",
                mib(warm),
                warm as f64 / n
            );
        }
        println!(
            "{:<20} {:>14} {:>14} {:>10.2} {:>12.2}",
            "TOTAL",
            self.ram_cold,
            self.ram_warm,
            mib(self.ram_warm),
            self.ram_warm as f64 / n
        );
    }

    fn print_table(&self) {
        println!("YSpot filename-index matching benchmark — SPEC §3.4 / §2.5 / §10 M0");
        println!("{}", "=".repeat(96));
        println!("{HEADER_CAVEAT}");
        println!("{}", "-".repeat(96));
        println!(
            "corpus        entries={} (requested {})  dirs={}  files={}  seed={:#x}  gen={:.0} ms",
            self.corpus_generated,
            self.entries_requested,
            self.corpus_dirs,
            self.corpus_files,
            self.seed,
            self.gen_ms
        );
        let per_sec = if self.build_ms > 0.0 {
            self.inserted as f64 / (self.build_ms / 1e3)
        } else {
            0.0
        };
        println!(
            "index build   inserted={}  {:.0} ms  ({:.0} entries/s)  [arena+hashmap only; NOT the \
             §3.2 enumeration budget]",
            self.inserted, self.build_ms, per_sec
        );
        println!(
            "              finalize (EntrySink::finish → shrink_to_fit): {:.0} ms",
            self.finalize_ms
        );
        let bpe = self.ram_warm as f64 / self.inserted.max(1) as f64;
        println!(
            "memory        ram_bytes cold={} ({:.1} MB)  warm+accel={} ({:.1} MB)  = {:.1} B/entry",
            self.ram_cold,
            mib(self.ram_cold),
            self.ram_warm,
            mib(self.ram_warm),
            bpe
        );
        println!(
            "              §3.4 budget for {} entries: typical ≤ {:.1} MB, hard cap ≤ {:.1} MB \
             → {}",
            self.inserted,
            self.ram_budget_typical / 1024.0 / 1024.0,
            self.ram_budget_cap / 1024.0 / 1024.0,
            verdict(self.ram_pass)
        );
        println!(
            "              mean name bytes L = {:.1} (folded {:.1}); arena payload, not capacity",
            self.name_arena_bytes as f64 / self.inserted.max(1) as f64,
            self.folded_arena_bytes as f64 / self.inserted.max(1) as f64,
        );
        println!("{RAM_ACCOUNTING_NOTE}");
        println!("{}", "-".repeat(96));
        self.print_ram_table();
        println!("{}", "-".repeat(96));
        println!("lazy accel-structure rebuild cost (§3.4 prefilters build on first use):");
        println!(
            "  cold first query (2-char, builds folded_order + initials arena) : {:>12.1} µs",
            self.cold_basic_us
        );
        println!(
            "  cold first query (3-char, additionally builds trigram postings) : {:>12.1} µs",
            self.cold_trigram_us
        );
        println!(
            "  after ONE UsnEvent::Create, next 2-char query (full rebuild)    : {:>12.1} µs",
            self.mutation_basic_us
        );
        println!(
            "  after ONE UsnEvent::Create, next 3-char query (full rebuild)    : {:>12.1} µs",
            self.mutation_trigram_us
        );
        println!(
            "  path_of() for {} results (p50/p95)                              : {:>12.1} / \
             {:.1} µs",
            self.max_results, self.path_of_stats.p50, self.path_of_stats.p95
        );
        println!("{}", "-".repeat(96));
        println!("{SCAN_PROBE_NOTE}");
        println!(
            "  arena {:.1} MB   needle {:?}   hits {} (must be 0, or the probe is invalid)",
            mib(self.folded_arena_bytes as u64),
            SCAN_PROBE_NEEDLE,
            self.scan_probe_hits
        );
        println!(
            "  p50 {:.1} µs   p95 {:.1} µs   p99 {:.1} µs   max {:.1} µs   = {:.2} GB/s",
            self.scan_probe_stats.p50,
            self.scan_probe_stats.p95,
            self.scan_probe_stats.p99,
            self.scan_probe_stats.max,
            self.scan_gb_per_s()
        );
        println!(
            "  same scan over a 24 MB (1M-entry) arena: {:.2} ms",
            self.scan_ms_at_1m()
        );
        println!("{}", "-".repeat(96));
        println!(
            "warm steady-state query latency — {} iterations/class, max_results={}, \
             budget = p95 ≤ {:.1} ms (§2.5)",
            self.iterations,
            self.max_results,
            BUDGET_P95_US / 1000.0
        );
        println!(
            "{:<18} {:>9} {:>9} {:>9} {:>9} {:>9} {:>8} {:>7}  note",
            "class", "min µs", "p50 µs", "p95 µs", "p99 µs", "max µs", "hits", "budget"
        );
        println!("{}", "-".repeat(96));
        for c in &self.classes {
            println!(
                "{:<18} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>8.1} {:>7}  {}",
                c.name,
                c.stats.min,
                c.stats.p50,
                c.stats.p95,
                c.stats.p99,
                c.stats.max,
                c.mean_hits,
                verdict(c.pass),
                c.note
            );
        }
        println!("{}", "-".repeat(96));
        for c in &self.classes {
            println!("  {:<16} queries: {}", c.name, c.queries.join(", "));
        }
        println!("{}", "-".repeat(96));
        println!(
            "OVERALL: {}  (advisory{})",
            verdict(self.overall_pass),
            if self.strict {
                "; --strict → non-zero exit on failure"
            } else {
                "; pass --strict to make this gating"
            }
        );
        println!(
            "'hits' is the mean returned result count, capped at max_results — it is NOT candidate \
             volume."
        );
        println!("M0 sign-off still needs Machine A/B numbers on a real ≥1M-file NTFS volume.");
    }

    fn to_json(&self) -> String {
        let mut s = String::with_capacity(4096);
        s.push('{');
        kv_str(&mut s, "schema", "yspot.bench.v1");
        s.push(',');
        kv_str(&mut s, "disclaimer", HEADER_CAVEAT);
        s.push(',');
        let _ = write!(
            s,
            "\"config\":{{\"entries_requested\":{},\"iterations\":{},\"seed\":{},\
             \"max_results\":{},\"strict\":{}}}",
            self.entries_requested, self.iterations, self.seed, self.max_results, self.strict
        );
        let _ = write!(
            s,
            ",\"corpus\":{{\"entries\":{},\"dirs\":{},\"files\":{},\"gen_ms\":{:.3}}}",
            self.corpus_generated, self.corpus_dirs, self.corpus_files, self.gen_ms
        );
        let per_sec = if self.build_ms > 0.0 {
            self.inserted as f64 / (self.build_ms / 1e3)
        } else {
            0.0
        };
        let n = self.inserted.max(1) as f64;
        let _ = write!(
            s,
            ",\"build\":{{\"inserted\":{},\"build_ms\":{:.3},\"finalize_ms\":{:.3},\
             \"entries_per_sec\":{:.0},\"ram_bytes_cold\":{},\"ram_bytes_warm\":{},\
             \"bytes_per_entry\":{:.2},\"name_arena_bytes\":{},\"folded_arena_bytes\":{},\
             \"mean_name_bytes\":{:.2},\"mean_folded_bytes\":{:.2},\
             \"budget_typical_bytes\":{:.0},\"budget_cap_bytes\":{:.0},\"pass\":{}}}",
            self.inserted,
            self.build_ms,
            self.finalize_ms,
            per_sec,
            self.ram_cold,
            self.ram_warm,
            self.ram_warm as f64 / n,
            self.name_arena_bytes,
            self.folded_arena_bytes,
            self.name_arena_bytes as f64 / n,
            self.folded_arena_bytes as f64 / n,
            self.ram_budget_typical,
            self.ram_budget_cap,
            self.ram_pass
        );
        s.push_str(",\"ram_breakdown\":{\"cold\":");
        ram_json(&mut s, &self.ram_cold_parts);
        s.push_str(",\"warm\":");
        ram_json(&mut s, &self.ram_warm_parts);
        s.push('}');
        s.push_str(",\"pass_a_scan_us\":{\"needle\":");
        json_str(SCAN_PROBE_NEEDLE, &mut s);
        let _ = write!(
            s,
            ",\"hits\":{},\"arena_bytes\":{},\"min\":{:.1},\"p50\":{:.1},\"p95\":{:.1},\
             \"p99\":{:.1},\"max\":{:.1},\"gb_per_s\":{:.3},\"ms_at_1m_reference\":{:.3}}}",
            self.scan_probe_hits,
            self.folded_arena_bytes,
            self.scan_probe_stats.min,
            self.scan_probe_stats.p50,
            self.scan_probe_stats.p95,
            self.scan_probe_stats.p99,
            self.scan_probe_stats.max,
            self.scan_gb_per_s(),
            self.scan_ms_at_1m()
        );
        let _ = write!(
            s,
            ",\"lazy_rebuild_us\":{{\"cold_basic\":{:.1},\"cold_trigrams\":{:.1},\
             \"after_one_usn_event_basic\":{:.1},\"after_one_usn_event_trigrams\":{:.1}}}",
            self.cold_basic_us,
            self.cold_trigram_us,
            self.mutation_basic_us,
            self.mutation_trigram_us
        );
        let _ = write!(
            s,
            ",\"path_of_us\":{{\"results\":{},\"min\":{:.1},\"p50\":{:.1},\"p95\":{:.1},\
             \"p99\":{:.1},\"max\":{:.1}}}",
            self.max_results,
            self.path_of_stats.min,
            self.path_of_stats.p50,
            self.path_of_stats.p95,
            self.path_of_stats.p99,
            self.path_of_stats.max
        );
        let _ = write!(s, ",\"budget_p95_us\":{BUDGET_P95_US:.1},\"classes\":[");
        for (i, c) in self.classes.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push('{');
            kv_str(&mut s, "name", c.name);
            s.push(',');
            kv_str(&mut s, "note", c.note);
            s.push_str(",\"queries\":[");
            for (j, q) in c.queries.iter().enumerate() {
                if j > 0 {
                    s.push(',');
                }
                json_str(q, &mut s);
            }
            let _ = write!(
                s,
                "],\"iterations\":{},\"min_us\":{:.1},\"p50_us\":{:.1},\"p95_us\":{:.1},\
                 \"p99_us\":{:.1},\"max_us\":{:.1},\"mean_hits\":{:.3},\"pass\":{}}}",
                self.iterations,
                c.stats.min,
                c.stats.p50,
                c.stats.p95,
                c.stats.p99,
                c.stats.max,
                c.mean_hits,
                c.pass
            );
        }
        let _ = write!(s, "],\"overall_pass\":{}}}", self.overall_pass);
        s
    }
}

fn mib(b: u64) -> f64 {
    b as f64 / 1024.0 / 1024.0
}

fn verdict(ok: bool) -> &'static str {
    if ok {
        "PASS"
    } else {
        "FAIL"
    }
}

/// One `RamBreakdown` as a JSON object, keyed exactly like the printed table.
fn ram_json(out: &mut String, b: &RamBreakdown) {
    out.push('{');
    for (i, (name, bytes)) in ram_rows(b).iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        json_str(&name.replace(':', "_"), out);
        let _ = write!(out, ":{bytes}");
    }
    let _ = write!(out, ",\"total\":{}}}", b.total());
}

fn kv_str(out: &mut String, k: &str, v: &str) {
    json_str(k, out);
    out.push(':');
    json_str(v, out);
}

fn json_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let n = c as u32;
                let _ = write!(out, "\\u{n:04x}");
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---------------------------------------------------------------------------
// Driver.
// ---------------------------------------------------------------------------

fn run(cfg: &Config) -> Report {
    let corpus = generate(cfg.entries, cfg.seed);
    let fuzzy_queries: Vec<String> = corpus
        .fuzzy_queries
        .iter()
        .take(QUERIES_PER_CLASS)
        .cloned()
        .collect();

    let mut ix = VolumeIndex::new(0, "C:\\".to_string());
    let t0 = Instant::now();
    for e in &corpus.entries {
        ix.add(e.frn, e.parent_frn, &e.name, e.flags);
    }
    let build_ms = ms(t0);
    // §3.2's sinks call finish() when enumeration ends; without it the bench
    // would measure an index still carrying bulk-load doubling slack that the
    // service never has.
    let t_fin = Instant::now();
    ix.finish();
    let finalize_ms = ms(t_fin);
    let inserted = ix.len();
    let ram_cold_parts = ix.ram_breakdown();
    let ram_cold = ram_cold_parts.total();
    let name_arena_bytes = ix.name_arena_len();
    let folded_arena_bytes = ix.folded_arena_len();

    // Query harvesting reads the arenas but never searches, so the cold-query
    // measurement below is still the first search this index has ever served.
    let queries = build_queries(&corpus.entries, &ix);

    // Sample FRNs for the path_of measurement before the corpus is dropped.
    let mut rng = Rng::new(cfg.seed ^ 0xA5A5_A5A5);
    let sample_frns: Vec<u64> = (0..cfg.max_results * 8)
        .map(|_| corpus.entries[rng.below(corpus.entries.len())].frn)
        .collect();
    let corpus_generated = corpus.entries.len();
    let (corpus_dirs, corpus_files, gen_ms) = (corpus.dirs, corpus.files, corpus.gen_ms);
    drop(corpus);

    // --- Lazy accel-structure cost, measured before anything is warm. -------
    // First search ever: pays Accel::ensure_basic (folded_order sort + the
    // initials arena). A 2-char query stops there — the fuzzy tier is skipped
    // below 3 bytes (§3.4).
    let cold2 = queries
        .initials2
        .first()
        .cloned()
        .unwrap_or_else(|| "co".to_string());
    let cold3 = queries
        .initials3
        .first()
        .cloned()
        .unwrap_or_else(|| "con".to_string());
    let t = Instant::now();
    std::hint::black_box(ix.search(&cold2, cfg.max_results, &|| false));
    let cold_basic_us = us(t);
    let t = Instant::now();
    std::hint::black_box(ix.search(&cold3, cfg.max_results, &|| false));
    let cold_trigram_us = us(t);

    // One USN create marks the accel dirty, which discards *everything* —
    // including the trigram postings. This is the per-keystroke-after-a-file-
    // change worst case, and it is a §2.5 risk in its own right.
    ix.apply(UsnEvent::Create {
        frn: frn_of(corpus_generated + 1),
        parent_frn: ROOT_FRN,
        name: "bench-mutation-probe.tmp".to_string(),
        flags: 0,
    });
    let t = Instant::now();
    std::hint::black_box(ix.search(&cold2, cfg.max_results, &|| false));
    let mutation_basic_us = us(t);
    let t = Instant::now();
    std::hint::black_box(ix.search(&cold3, cfg.max_results, &|| false));
    let mutation_trigram_us = us(t);

    let ram_warm_parts = ix.ram_breakdown();
    let ram_warm = ram_warm_parts.total();

    // --- Isolated Pass-A scan floor ----------------------------------------
    // One memmem over the folded arena with a needle that cannot hit, so the
    // scan runs end to end and nothing maps, scores or ranks. Measured warm
    // and away from the cold-query probes above, which it would otherwise
    // pull the arena into cache for.
    for _ in 0..3 {
        std::hint::black_box(ix.arena_scan_probe(SCAN_PROBE_NEEDLE));
    }
    let mut scan_samples = Vec::with_capacity(cfg.iterations);
    let mut scan_probe_hits = 0usize;
    for _ in 0..cfg.iterations {
        let t = Instant::now();
        let hits = ix.arena_scan_probe(SCAN_PROBE_NEEDLE);
        scan_samples.push(us(t));
        // Per scan, not a running total: any non-zero invalidates the probe.
        scan_probe_hits = std::hint::black_box(hits);
    }

    // --- path_of for one result page ---------------------------------------
    let mut path_samples = Vec::with_capacity(cfg.iterations);
    for i in 0..cfg.iterations {
        let base = (i * cfg.max_results) % sample_frns.len();
        let t = Instant::now();
        for k in 0..cfg.max_results {
            let frn = sample_frns[(base + k) % sample_frns.len()];
            std::hint::black_box(ix.path_of(frn));
        }
        path_samples.push(us(t));
    }

    // --- Per-class steady-state latency ------------------------------------
    let it = cfg.iterations;
    let mr = cfg.max_results;
    let classes = vec![
        measure_class(&ix, "exact", "tier 1.0", queries.exact, it, mr),
        measure_class(&ix, "prefix", "tier 0.9, 6 chars", queries.prefix, it, mr),
        measure_class(
            &ix,
            "word-boundary",
            "tier 0.8",
            queries.word_boundary,
            it,
            mr,
        ),
        measure_class(
            &ix,
            "initials-2",
            "tier 0.7; §10 M0 2-char worst case (no fuzzy tier below 3 bytes)",
            queries.initials2,
            it,
            mr,
        ),
        measure_class(
            &ix,
            "initials-3",
            "tier 0.7; §10 M0 3-char worst case (single trigram → widest posting list)",
            queries.initials3,
            it,
            mr,
        ),
        measure_class(&ix, "substring", "tier 0.55", queries.substring, it, mr),
        measure_class(
            &ix,
            "fuzzy-subseq",
            "tier 0.3-0.5; planted family, see plant_fuzzy_name",
            fuzzy_queries,
            it,
            mr,
        ),
        measure_class(
            &ix,
            "common-substr",
            "worst case for candidate volume",
            COMMON_SUBSTRINGS.iter().map(|s| s.to_string()).collect(),
            it,
            mr,
        ),
        measure_class(
            &ix,
            "no-match",
            "trigram early-out",
            NO_MATCH_QUERIES.iter().map(|s| s.to_string()).collect(),
            it,
            mr,
        ),
    ];

    let scale = inserted as f64 / 1_000_000.0;
    let ram_budget_typical = RAM_TYPICAL_PER_1M * scale;
    let ram_budget_cap = RAM_CAP_PER_1M * scale;
    let ram_pass = (ram_warm as f64) <= ram_budget_cap;
    let overall_pass = ram_pass && classes.iter().all(|c| c.pass);

    Report {
        entries_requested: cfg.entries,
        iterations: cfg.iterations,
        seed: cfg.seed,
        max_results: cfg.max_results,
        strict: cfg.strict,
        corpus_generated,
        corpus_dirs,
        corpus_files,
        gen_ms,
        inserted,
        build_ms,
        finalize_ms,
        ram_cold,
        ram_warm,
        ram_cold_parts,
        ram_warm_parts,
        name_arena_bytes,
        folded_arena_bytes,
        ram_budget_typical,
        ram_budget_cap,
        ram_pass,
        cold_basic_us,
        cold_trigram_us,
        mutation_basic_us,
        mutation_trigram_us,
        scan_probe_stats: stats(scan_samples),
        scan_probe_hits,
        path_of_stats: stats(path_samples),
        classes,
        overall_pass,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cfg = match parse_args(&args) {
        Ok(Some(c)) => c,
        Ok(None) => {
            println!("{USAGE}");
            return;
        }
        Err(e) => {
            eprintln!("bench: {e}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };

    let report = run(&cfg);
    let json = report.to_json();

    if let Some(path) = &cfg.json_out {
        if let Err(e) = std::fs::write(path, &json) {
            eprintln!("bench: cannot write {path}: {e}");
            std::process::exit(2);
        }
    }
    if cfg.json {
        println!("{json}");
    } else {
        report.print_table();
    }

    if cfg.strict && !report.overall_pass {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rng_is_deterministic_and_never_sticks() {
        let mut a = Rng::new(0);
        let mut b = Rng::new(0);
        let first: Vec<u64> = (0..8).map(|_| a.next_u64()).collect();
        let second: Vec<u64> = (0..8).map(|_| b.next_u64()).collect();
        assert_eq!(first, second);
        assert!(first.windows(2).all(|w| w[0] != w[1]));
    }

    #[test]
    fn corpus_is_reproducible_and_shaped() {
        let a = generate(4_000, 7);
        let b = generate(4_000, 7);
        assert_eq!(a.entries.len(), b.entries.len());
        for (x, y) in a.entries.iter().zip(b.entries.iter()) {
            assert_eq!(x.name, y.name);
            assert_eq!(
                (x.frn, x.parent_frn, x.flags),
                (y.frn, y.parent_frn, y.flags)
            );
        }
        // ~15% directories, and FRNs are unique.
        let ratio = a.dirs as f64 / a.entries.len() as f64;
        assert!((0.10..0.30).contains(&ratio), "dir ratio {ratio}");
        let mut frns: Vec<u64> = a.entries.iter().map(|e| e.frn).collect();
        frns.sort_unstable();
        let before = frns.len();
        frns.dedup();
        assert_eq!(frns.len(), before);
        assert!(!a.fuzzy_queries.is_empty());
        // Non-ASCII names must actually be present (Unicode fold path).
        assert!(a.entries.iter().any(|e| !e.name.is_ascii()));
    }

    #[test]
    fn every_query_class_hits_the_corpus() {
        let corpus = generate(30_000, 3);
        let mut ix = VolumeIndex::new(0, "C:\\".to_string());
        for e in &corpus.entries {
            ix.add(e.frn, e.parent_frn, &e.name, e.flags);
        }
        let q = build_queries(&corpus.entries, &ix);
        let classes: Vec<(&str, &Vec<String>)> = vec![
            ("exact", &q.exact),
            ("prefix", &q.prefix),
            ("word_boundary", &q.word_boundary),
            ("initials2", &q.initials2),
            ("initials3", &q.initials3),
            ("substring", &q.substring),
            ("fuzzy", &corpus.fuzzy_queries),
        ];
        for (name, qs) in classes {
            assert!(!qs.is_empty(), "{name}: no queries generated");
            for s in qs {
                assert!(
                    !ix.search(s, 32, &|| false).is_empty(),
                    "{name}: query {s:?} matched nothing"
                );
            }
        }
        for s in NO_MATCH_QUERIES {
            assert!(ix.search(s, 32, &|| false).is_empty(), "{s:?} should miss");
        }
    }

    #[test]
    fn segment_starts_mirrors_matching_rules() {
        assert_eq!(segment_starts("FooBar.txt"), vec![0, 3, 7]);
        assert_eq!(segment_starts("my-file.txt"), vec![0, 3, 8]);
        assert_eq!(segment_starts("..."), Vec::<usize>::new());
        assert_eq!(initials_of("DataLoaderService.rs"), "dlsr");
    }

    #[test]
    fn planted_fuzzy_names_are_gapped_not_contiguous() {
        let mut rng = Rng::new(1);
        for i in 0..FUZZY_STEMS.len() {
            let (name, query) = plant_fuzzy_name(&mut rng, i);
            let folded = yspot_index::index::fold(&name);
            assert!(
                !folded.contains(query.as_str()),
                "{query} is contiguous in {name}"
            );
            // …but still a subsequence, which is what the fuzzy tier scores.
            let mut it = folded.chars();
            assert!(
                query.chars().all(|c| it.any(|n| n == c)),
                "{query} is not a subsequence of {name}"
            );
        }
    }

    #[test]
    fn stats_percentiles() {
        let s = stats((1..=100).map(f64::from).collect());
        assert_eq!(s.min, 1.0);
        assert_eq!(s.p50, 50.0);
        assert_eq!(s.p95, 95.0);
        assert_eq!(s.p99, 99.0);
        assert_eq!(s.max, 100.0);
        let empty = stats(Vec::new());
        assert_eq!(empty.p95, 0.0);
        assert_eq!(empty.p99, 0.0);
    }

    #[test]
    fn scan_probe_needle_misses_but_the_probe_sees_the_arena() {
        let corpus = generate(30_000, 11);
        let mut ix = VolumeIndex::new(0, "C:\\".to_string());
        for e in &corpus.entries {
            ix.add(e.frn, e.parent_frn, &e.name, e.flags);
        }
        ix.finish();
        // Zero hits is the whole point: the scan must run to the end of the
        // arena, and a probe that matched would measure something else.
        assert_eq!(ix.arena_scan_probe(SCAN_PROBE_NEEDLE), 0);
        // …and the probe is reading the real arena, not returning 0 blindly.
        let name = ix.name_of(corpus.entries[0].frn).unwrap().to_string();
        assert!(ix.arena_scan_probe(&name) >= 1);
    }

    #[test]
    fn arg_parsing() {
        let c = parse_args(&v(&[
            "--entries",
            "300_000",
            "--iterations",
            "50",
            "--strict",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(c.entries, 300_000);
        assert_eq!(c.iterations, 50);
        assert!(c.strict);
        assert!(!c.json);
        assert_eq!(c.max_results, DEFAULT_MAX_RESULTS);
        // The §10 M0 target size, in both spellings a CI script might use.
        for spelling in ["1000000", "1_000_000"] {
            let c = parse_args(&v(&["--entries", spelling])).unwrap().unwrap();
            assert_eq!(c.entries, 1_000_000);
        }
        assert!(parse_args(&v(&["--help"])).unwrap().is_none());
        assert!(parse_args(&v(&["--entries"])).is_err());
        assert!(parse_args(&v(&["--nope"])).is_err());
        // Corpus generation needs a minimum to build a tree at all.
        assert_eq!(
            parse_args(&v(&["--entries", "1"]))
                .unwrap()
                .unwrap()
                .entries,
            1_000
        );
    }

    #[test]
    fn json_is_escaped_and_well_formed_enough() {
        let mut s = String::new();
        json_str("a\"b\\c\nd\u{1}é", &mut s);
        assert_eq!(s, "\"a\\\"b\\\\c\\nd\\u0001é\"");
    }

    #[test]
    fn end_to_end_small_run() {
        let cfg = Config {
            entries: 5_000,
            iterations: 5,
            ..Config::default()
        };
        let r = run(&cfg);
        assert_eq!(r.inserted, r.corpus_generated);
        assert!(r.ram_warm >= r.ram_cold);
        assert_eq!(r.classes.len(), 9);
        assert!(r.classes.iter().all(|c| !c.queries.is_empty()));
        // The breakdown must account for every reported byte, or §4.3's
        // attribution is fiction.
        assert_eq!(r.ram_cold_parts.total(), r.ram_cold);
        assert_eq!(r.ram_warm_parts.total(), r.ram_warm);
        assert_eq!(r.scan_probe_hits, 0);
        assert!(r.name_arena_bytes > 0 && r.folded_arena_bytes > 0);
        let json = r.to_json();
        assert!(json.starts_with('{') && json.ends_with('}'));
        assert!(json.contains("\"overall_pass\""));
        assert!(json.contains("\"ram_breakdown\""));
        assert!(json.contains("\"pass_a_scan_us\""));
        assert!(json.contains("\"p99_us\""));
    }
}
