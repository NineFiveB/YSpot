//! Built-in shell commands (SPEC.md §5.9, §7.6).
//!
//! Things YSpot itself does, reachable by typing their name rather than by
//! hunting for the tray icon — §5.9 requires Settings to open "from the tray
//! or the `settings` command", and a launcher whose own settings can only be
//! reached through a system-tray menu is a launcher that failed at its one
//! job.
//!
//! The list is static and tiny on purpose. §7.6's system commands (lock,
//! sleep, restart) need privilege and confirmation handling of their own and
//! are not here yet; this module is the shape they will land in.

use crate::matcher::{self, Ranges, Target};

/// One built-in command.
pub struct Command {
    pub id: &'static str,
    pub name: &'static str,
    /// What the row shows under the name. "YSpot" for the things YSpot does
    /// itself; "Settings" for the Windows Settings home, which is a YSpot
    /// command that lands in another app and must not claim to be a YSpot
    /// page (§7.2 amendment).
    pub subtitle: &'static str,
    target: Target,
    synonyms: Vec<Target>,
}

/// A scored command hit, in the shape the frontend row needs.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandMatch {
    pub id: String,
    pub name: String,
    pub subtitle: &'static str,
    pub score: f32,
    pub match_ranges: Ranges,
}

pub const MAX_RESULTS: usize = 3;

/// Built-ins sit one band above the shared match scale (§5.11 r2 amendment).
///
/// There are five of them, hand-written, and none is reachable by accident:
/// the promotion needs two characters AND a match on the command's own NAME
/// at `WORD_START` or better. What it buys is that a user who types the name
/// of a thing YSpot does gets that thing, instead of losing to whatever the
/// filesystem happens to contain. On the author's machine 101 directories
/// under one user profile are named exactly `Settings` — 86 of them inside
/// `%LOCALAPPDATA%\Packages` — and EXACT times a shallow depth penalty puts
/// each at 0.89-0.91 against a command's flat 0.800. The command lost.
pub const TIER_BONUS: f32 = 0.5;
/// The band, spelled out so the invariant is a comparison rather than const
/// float arithmetic: `matcher::WORD_START + TIER_BONUS` and
/// `matcher::EXACT + TIER_BONUS`. Held to those by a test.
pub const BAND_FLOOR: f32 = 1.3;
pub const BAND_CEIL: f32 = 1.5;
/// Above every other scored row (EXACT plus frecency's ceiling) and below the
/// calculator's flat 2.0, which stays first (§7.7). Compile-time, so the band
/// cannot be narrowed into an overlap by editing any one of these constants.
const _: () = assert!(BAND_FLOOR > matcher::EXACT + crate::frecency::MAX_BONUS);
const _: () = assert!(BAND_CEIL < 2.0);

/// The catalog, built once at startup.
pub fn all() -> Vec<Command> {
    [
        (
            "yspot.settings",
            "YSpot Settings",
            "YSpot",
            &["preferences", "options", "hotkey", "configure"][..],
        ),
        // §7.2: the Settings HOME is not one of ninety-odd deep links, it is
        // the destination people mean when they type "settings", so it lives
        // here rather than in the data catalog. Deliberately NOT synonymed
        // "control" or "control panel": those name the other application, and
        // the catalog's twenty Control Panel items already answer for it.
        (
            "windows.settings",
            "Windows Settings",
            "Settings",
            &["system settings", "pc settings", "windows settings"][..],
        ),
        (
            "yspot.clipboard",
            "Clipboard History",
            "YSpot",
            &["paste", "clipboard", "copied", "history"][..],
        ),
        (
            "yspot.files",
            "File Search",
            "YSpot",
            &["files", "find file", "search files", "documents"][..],
        ),
        (
            "yspot.quit",
            "Quit YSpot",
            "YSpot",
            &["exit", "close yspot"][..],
        ),
    ]
    .into_iter()
    .map(|(id, name, subtitle, synonyms)| Command {
        id,
        name,
        subtitle,
        target: Target::new(name),
        synonyms: synonyms.iter().map(|s| Target::new(s)).collect(),
    })
    .collect()
}

pub fn match_query(commands: &[Command], query: &str, max: usize) -> Vec<CommandMatch> {
    let q = matcher::fold_query(query);
    // Two characters before a command shows up: these are always-present
    // rows, and one letter should not push a file result off the page.
    if q.len() < 2 || max == 0 {
        return Vec::new();
    }
    let mut hits: Vec<(f32, &Command, Ranges)> = commands
        .iter()
        .filter_map(|c| {
            let (score, ranges) = matcher::score_with_synonyms(&c.target, &c.synonyms, &q)?;
            // The band is for "the user typed this command's NAME". A synonym
            // hit is a weaker claim and stays on the shared scale — otherwise
            // `documents` would put File Search above the user's Documents
            // folder, and `paste` above a file called paste.txt.
            let direct = matcher::score(&c.target, &q).map_or(0.0, |(s, _)| s);
            let score = if direct >= matcher::WORD_START {
                score + TIER_BONUS
            } else {
                score
            };
            Some((score, c, ranges))
        })
        .collect();
    hits.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.name.cmp(b.1.name)));
    hits.truncate(max);
    hits.into_iter()
        .map(|(score, c, match_ranges)| CommandMatch {
            id: c.id.to_string(),
            name: c.name.to_string(),
            subtitle: c.subtitle,
            score,
            match_ranges,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The band is a claim about where built-ins sit relative to everything
    /// else. Spelled as comparisons against the real constants, so a change to
    /// any of them breaks this rather than silently moving the band.
    #[test]
    fn the_band_sits_between_every_scored_row_and_the_calculator() {
        assert_eq!(matcher::WORD_START + TIER_BONUS, BAND_FLOOR);
        assert_eq!(matcher::EXACT + TIER_BONUS, BAND_CEIL);
        // The two ends of the band — above the best any other row can reach,
        // below the calculator — are asserted at COMPILE time next to the
        // constants themselves, so they cannot be edited into an overlap.
    }

    /// The whole point of the band. On the author's machine the shallowest
    /// directory named `Settings` scores about 0.91 — EXACT times a depth
    /// penalty — and a command's flat WORD_START 0.800 lost to it, so typing
    /// "settings" put a package folder above YSpot's own settings page.
    #[test]
    fn a_builtin_outranks_the_deepest_exact_file_match() {
        let hits = match_query(&all(), "settings", MAX_RESULTS);
        assert!(
            hits[0].score > 1.0,
            "a built-in scored {} — a file named Settings reaches ~0.91",
            hits[0].score
        );
    }

    /// What the user asked for: typing "settings" surfaces the two settings
    /// destinations, not the filesystem's opinion of the word.
    #[test]
    fn both_settings_destinations_are_on_top_for_settings() {
        let names: Vec<String> = match_query(&all(), "settings", MAX_RESULTS)
            .into_iter()
            .map(|m| m.name)
            .collect();
        assert_eq!(names[0], "Windows Settings");
        assert_eq!(names[1], "YSpot Settings");
    }

    /// The guard that keeps the band out of reach of a substring or fuzzy
    /// accident. "lipboard" still finds Clipboard History, on the shared
    /// scale, where it belongs.
    #[test]
    fn the_promotion_stops_below_word_start() {
        let hits = match_query(&all(), "lipboard", MAX_RESULTS);
        assert_eq!(hits[0].name, "Clipboard History");
        assert!(
            hits[0].score < matcher::WORD_START,
            "a substring hit was promoted: {}",
            hits[0].score
        );
    }

    /// A synonym is a weaker claim than a name. Without this, "documents"
    /// would put File Search above the user's own Documents folder.
    #[test]
    fn a_synonym_hit_is_not_promoted() {
        let hits = match_query(&all(), "documents", MAX_RESULTS);
        assert_eq!(hits[0].name, "File Search");
        assert!(
            hits[0].score < BAND_FLOOR,
            "a synonym hit reached the band: {}",
            hits[0].score
        );
    }

    /// A command with no dispatch arm registers, ranks, and then does nothing
    /// at Enter. This reads the source text, so it proves the arm was WRITTEN
    /// rather than that it works — `execute_action` needs an `AppHandle` and
    /// is not reachable from a unit test. It is still the only thing standing
    /// between a renamed id and a silently dead command.
    #[test]
    fn every_command_has_a_dispatch_arm() {
        let lib = include_str!("lib.rs");
        for c in all() {
            assert!(
                lib.contains(&format!("\"{}\" =>", c.id)),
                "no dispatch arm for {}",
                c.id
            );
        }
    }

    #[test]
    fn settings_is_reachable_by_name_and_by_synonym() {
        let cmds = all();
        let names = |q: &str| -> Vec<String> {
            match_query(&cmds, q, MAX_RESULTS)
                .into_iter()
                .map(|m| m.name)
                .collect()
        };
        // §5.9's requirement, literally.
        assert!(names("settings").contains(&"YSpot Settings".to_string()));
        assert!(names("preferences").contains(&"YSpot Settings".to_string()));
        assert!(names("hotkey").contains(&"YSpot Settings".to_string()));
        assert!(names("quit").contains(&"Quit YSpot".to_string()));
        // §7.4's history is reached the same way.
        assert!(names("clipboard").contains(&"Clipboard History".to_string()));
        assert!(names("paste").contains(&"Clipboard History".to_string()));
    }

    #[test]
    fn one_character_and_nonsense_match_nothing() {
        let cmds = all();
        assert!(match_query(&cmds, "s", MAX_RESULTS).is_empty());
        assert!(match_query(&cmds, "", MAX_RESULTS).is_empty());
        assert!(match_query(&cmds, "zzqxjv", MAX_RESULTS).is_empty());
    }

    #[test]
    fn every_command_has_a_unique_id_and_matches_its_own_name() {
        let cmds = all();
        let mut ids: Vec<&str> = cmds.iter().map(|c| c.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "duplicate command id");
        for c in &cmds {
            let hits = match_query(&cmds, c.name, MAX_RESULTS);
            assert!(
                hits.iter().any(|h| h.id == c.id),
                "{} does not match its own name",
                c.id
            );
        }
    }
}
