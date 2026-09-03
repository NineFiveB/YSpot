//! The shell-side name matcher shared by every catalog source (§7.1, §7.2).
//!
//! Scores on the same tier scale as the file index (§3.4) — exact 1.0,
//! prefix 0.9, word start 0.8, initials 0.7, substring 0.55, subsequence
//! 0.3–0.5 by density — so apps, settings pages and files can be interleaved
//! under one global score (§5.11, §7.3) rather than each source inventing a
//! scale and the merge guessing at how they compare.
//!
//! A [`Target`] precomputes everything scoring needs: the folded characters,
//! the UTF-16 offset of each character (§5.13 wants ranges in code units),
//! and where each segment starts. That work happens once when a catalog is
//! built, never per keystroke.

/// UTF-16 code-unit ranges into the display text (§5.13).
pub type Ranges = Vec<(u32, u32)>;

/// Tier bases, identical to the index's (§3.4).
pub const EXACT: f32 = 1.0;
pub const PREFIX: f32 = 0.9;
pub const WORD_START: f32 = 0.8;
pub const INITIALS: f32 = 0.7;
pub const SUBSTRING: f32 = 0.55;

/// One matchable string, prepared for scoring.
#[derive(Clone, Debug)]
pub struct Target {
    /// One folded char per original char, so a match position maps straight
    /// back to the display text.
    folded: Vec<char>,
    /// UTF-16 offset of each original char, plus the total.
    utf16_offsets: Vec<u32>,
    /// Char indexes where a segment starts.
    segment_starts: Vec<u32>,
}

impl Target {
    pub fn new(text: &str) -> Target {
        let folded: Vec<char> = text.chars().map(fold_char).collect();
        let mut utf16_offsets = Vec::with_capacity(folded.len() + 1);
        let mut off = 0u32;
        for c in text.chars() {
            utf16_offsets.push(off);
            off += c.len_utf16() as u32;
        }
        utf16_offsets.push(off);
        Target {
            segment_starts: segment_starts(text),
            folded,
            utf16_offsets,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.folded.is_empty()
    }

    fn range(&self, start: usize, len: usize) -> (u32, u32) {
        (self.utf16_offsets[start], self.utf16_offsets[start + len])
    }
}

/// One folded char per input char: lowercase, keeping only the first char of
/// a multi-char expansion (`İ` → `i`) so positions stay aligned with the
/// display text. The file index folds with full NFC plus lowercase; catalog
/// names are short and the difference is invisible for matching.
pub fn fold_char(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

pub fn fold_query(q: &str) -> Vec<char> {
    q.trim().chars().map(fold_char).collect()
}

fn is_sep(c: char) -> bool {
    matches!(
        c,
        ' ' | '-' | '_' | '.' | '/' | '\\' | '(' | ')' | '&' | '+' | ':' | ','
    )
}

/// Segment starts by the same rule the index uses for initials: after a
/// separator, and at a lower→upper camel transition.
fn segment_starts(text: &str) -> Vec<u32> {
    let mut out = Vec::new();
    let mut prev: Option<char> = None;
    for (i, c) in text.chars().enumerate() {
        let start = match prev {
            None => !is_sep(c),
            Some(p) => (is_sep(p) && !is_sep(c)) || (p.is_lowercase() && c.is_uppercase()),
        };
        if start {
            out.push(i as u32);
        }
        prev = Some(c);
    }
    out
}

/// Score `target` against an already-folded query, with the ranges to
/// highlight; `None` when it does not match at all.
pub fn score(target: &Target, q: &[char]) -> Option<(f32, Ranges)> {
    if q.is_empty() || target.is_empty() {
        return None;
    }
    let n = target.folded.len();
    // Contiguous occurrences; the first per tier is all that matters.
    let mut first_sub: Option<usize> = None;
    let mut first_word: Option<usize> = None;
    if q.len() <= n {
        for start in 0..=n - q.len() {
            if target.folded[start..start + q.len()] == *q {
                if first_sub.is_none() {
                    first_sub = Some(start);
                }
                if target.segment_starts.contains(&(start as u32)) {
                    first_word = Some(start);
                    break;
                }
            }
        }
    }
    if first_sub == Some(0) {
        let base = if q.len() == n { EXACT } else { PREFIX };
        return Some((base, vec![target.range(0, q.len())]));
    }
    if let Some(s) = first_word {
        return Some((WORD_START, vec![target.range(s, q.len())]));
    }
    // Initials: the query is a prefix of the segment-initial sequence.
    if q.len() >= 2 && q.len() <= target.segment_starts.len() {
        let ok = target
            .segment_starts
            .iter()
            .zip(q)
            .all(|(&s, &qc)| target.folded[s as usize] == qc);
        if ok {
            let ranges = target.segment_starts[..q.len()]
                .iter()
                .map(|&s| target.range(s as usize, 1))
                .collect();
            return Some((INITIALS, ranges));
        }
    }
    if let Some(s) = first_sub {
        return Some((SUBSTRING, vec![target.range(s, q.len())]));
    }
    // Subsequence (fuzzy), 3+ chars like the index: density = qlen / span.
    if q.len() >= 3 {
        let mut positions = Vec::with_capacity(q.len());
        let mut qi = 0;
        for (i, &c) in target.folded.iter().enumerate() {
            if qi < q.len() && c == q[qi] {
                positions.push(i);
                qi += 1;
            }
        }
        if qi == q.len() {
            let span = positions[q.len() - 1] - positions[0] + 1;
            let density = q.len() as f32 / span as f32;
            let ranges = positions.iter().map(|&p| target.range(p, 1)).collect();
            return Some((0.3 + 0.2 * density, ranges));
        }
    }
    None
}

/// Best score across a primary target and any number of alternates
/// (synonyms, keywords). Only the primary carries highlight ranges: a hit on
/// a synonym has nothing to underline in the name being shown, and inventing
/// ranges for it would highlight the wrong characters.
pub fn score_with_synonyms(
    primary: &Target,
    synonyms: &[Target],
    q: &[char],
) -> Option<(f32, Ranges)> {
    let direct = score(primary, q);
    // A synonym hit is worth less than the same tier on the visible name,
    // so an exact synonym never outranks an exact name.
    let via_synonym = synonyms
        .iter()
        .filter_map(|t| score(t, q).map(|(s, _)| s * 0.9))
        .max_by(f32::total_cmp);
    match (direct, via_synonym) {
        (Some((ds, r)), Some(ss)) if ss > ds => Some((ss, r)),
        (Some(hit), _) => Some(hit),
        (None, Some(ss)) => Some((ss, Vec::new())),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(text: &str, q: &str) -> Option<(f32, Ranges)> {
        score(&Target::new(text), &fold_query(q))
    }

    #[test]
    fn tiers_in_order() {
        assert_eq!(s("Notepad", "notepad").unwrap().0, EXACT);
        assert_eq!(s("Notepad", "note").unwrap().0, PREFIX);
        assert_eq!(s("Visual Studio Code", "code").unwrap().0, WORD_START);
        assert_eq!(s("Visual Studio Code", "vsc").unwrap().0, INITIALS);
        assert_eq!(s("Notepad", "tep").unwrap().0, SUBSTRING);
        let (score, ranges) = s("Visual Studio Code", "vslc").unwrap();
        assert!(score > 0.3 && score < 0.5, "fuzzy {score}");
        assert_eq!(ranges.len(), 4);
        assert!(s("Notepad", "xyz").is_none());
        assert!(s("Notepad", "").is_none());
    }

    #[test]
    fn ranges_are_utf16_and_track_the_display_text() {
        assert_eq!(s("Visual Studio Code", "code").unwrap().1, vec![(14, 18)]);
        assert_eq!(
            s("Visual Studio Code", "vsc").unwrap().1,
            vec![(0, 1), (7, 8), (14, 15)]
        );
        // A supplementary-plane char is two UTF-16 units.
        let (tier, ranges) = s("𝄞 Music", "music").unwrap();
        assert_eq!(tier, WORD_START);
        assert_eq!(ranges, vec![(3, 8)]);
        assert_eq!(s("PowerToys", "toys").unwrap().0, WORD_START);
        assert_eq!(s("PowerToys", "PT").unwrap().0, INITIALS);
    }

    #[test]
    fn separators_start_segments() {
        // Settings-page names lean on these.
        assert_eq!(s("Bluetooth & devices", "devices").unwrap().0, WORD_START);
        assert_eq!(
            s("Privacy: microphone", "microphone").unwrap().0,
            WORD_START
        );
        assert_eq!(s("Wi-Fi", "fi").unwrap().0, WORD_START);
    }

    #[test]
    fn synonyms_match_but_never_outrank_the_visible_name() {
        let name = Target::new("Bluetooth & devices");
        let syns = [Target::new("printers"), Target::new("mouse")];
        // A synonym hit scores, with no ranges to highlight on the name.
        let (score, ranges) = score_with_synonyms(&name, &syns, &fold_query("printers")).unwrap();
        assert!((score - EXACT * 0.9).abs() < 1e-6, "{score}");
        assert!(ranges.is_empty());
        // An exact name match still wins against an exact synonym.
        let exact = score_with_synonyms(&name, &syns, &fold_query("bluetooth & devices"))
            .unwrap()
            .0;
        assert!(exact > score);
        // No hit anywhere is no hit.
        assert!(score_with_synonyms(&name, &syns, &fold_query("zzz")).is_none());
    }
}
