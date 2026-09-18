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
    /// page (§7.2 amendment); "App" for one that opens an application.
    pub subtitle: &'static str,
    /// The AUMID this command opens, when it opens an app. The row is offered
    /// only while that app is in `AppsFolder`: §7.2 gates catalog pages on
    /// what the machine has, and a built-in that bypassed that would be a
    /// top row whose Enter fails on Windows 10, which this spec supports.
    pub requires_app: Option<&'static str>,
    /// The weakest match on the NAME that still takes the band. `WORD_START`
    /// for YSpot's own features, so `history` reaches Clipboard History.
    /// `PREFIX` for an app, so the band is reached only by typing toward its
    /// name from the start: `ba` word-starts "Windows Backup" and would
    /// otherwise open it from two letters, above Background and Battle.net.
    pub band_from: f32,
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
    /// Position in [`all`]. Two built-ins can tie exactly — `windows` reaches
    /// both Windows Settings and Windows Backup at PREFIX plus the band — and
    /// the list is written in the order they should show, so the frontend
    /// breaks that one tie by this rather than by name, which would put
    /// Backup above Settings on the strength of a B.
    pub order: u32,
}

pub const MAX_RESULTS: usize = 3;

/// Built-ins sit one band above the shared match scale (§5.11 r2 amendment).
///
/// There are six of them, hand-written, and none is reachable by accident:
/// the promotion needs two characters AND a match on the command's own NAME
/// at its `band_from` or better — `WORD_START` for YSpot's own features,
/// `PREFIX` for an app. What it buys is that a user who types the name
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

/// A thing YSpot does itself: always offered, in the band from `WORD_START`.
fn own(id: &'static str, name: &'static str, synonyms: &[&str]) -> Command {
    Command {
        id,
        name,
        subtitle: "YSpot",
        requires_app: None,
        band_from: matcher::WORD_START,
        target: Target::new(name),
        synonyms: synonyms.iter().map(|s| Target::new(s)).collect(),
    }
}

/// The catalog, built once at startup. Declaration order is display order
/// when two of these tie exactly, so it is not arbitrary.
pub fn all() -> Vec<Command> {
    vec![
        own(
            "yspot.settings",
            "YSpot Settings",
            &["preferences", "options", "hotkey", "configure"],
        ),
        // §7.2: the Settings HOME is not one of ninety-odd deep links, it is
        // the destination people mean when they type "settings", so it lives
        // here rather than in the data catalog. Deliberately NOT synonymed
        // "control" or "control panel": those name the other application, and
        // the catalog's twenty Control Panel items already answer for it.
        Command {
            subtitle: "Settings",
            ..own(
                "windows.settings",
                "Windows Settings",
                &["system settings", "pc settings", "windows settings"],
            )
        },
        // The Windows Backup app, directly under the Settings home. The ask
        // was that it sit there; on the shared scale it could not. `windows`
        // matched it PREFIX at 0.9 alongside eleven other "Windows …" apps
        // and four catalog pages, and by name it fell to row 4 — twice, since
        // the `ms-settings:sync` page is "Windows backup" too. A general
        // tie-break that put pages first got it to row 2 and put Taskbar
        // above Task Manager, Notifications above Notepad and Camera privacy
        // above Camera; demoting the C:\Windows row just promoted the next
        // directory named Windows. This list is the mechanism the codebase
        // has for "a hand-written destination the user typed the name of",
        // and it is what put Windows Settings itself here. Its app row is
        // suppressed by AUMID for the same reason the Settings app's is.
        //
        // The cost, in full: every prefix of "Windows" from `wi` up takes
        // the band, exactly as it already did for Windows Settings, so a
        // user typing `win` toward WinRAR or Windows Terminal is one row
        // further from it. The band is reached from PREFIX only — `ba`
        // word-starts the name and would otherwise open this from two
        // letters — and the row exists only while the app does.
        Command {
            subtitle: "App",
            requires_app: Some(crate::apps::WINDOWS_BACKUP_AUMID),
            band_from: matcher::PREFIX,
            ..own(
                "windows.backup",
                "Windows Backup",
                &["back up", "restore pc", "backup"],
            )
        },
        own(
            "yspot.clipboard",
            "Clipboard History",
            &["paste", "clipboard", "copied", "history"],
        ),
        own(
            "yspot.files",
            "File Search",
            &["files", "find file", "search files", "documents"],
        ),
        own("yspot.quit", "Quit YSpot", &["exit", "close yspot"]),
    ]
}

/// `has_app` answers whether an AUMID is in `AppsFolder` right now. It is a
/// query-time question because the app catalog is empty at startup and fills
/// on its own thread; a command gated at registration would be gated out on
/// every machine, and one never gated would be a dead row on Windows 10.
pub fn match_query(
    commands: &[Command],
    query: &str,
    max: usize,
    has_app: impl Fn(&str) -> bool,
) -> Vec<CommandMatch> {
    let q = matcher::fold_query(query);
    // Two characters before a command shows up: these are always-present
    // rows, and one letter should not push a file result off the page.
    if q.len() < 2 || max == 0 {
        return Vec::new();
    }
    let mut hits: Vec<(f32, u32, &Command, Ranges)> = commands
        .iter()
        .enumerate()
        .filter(|(_, c)| c.requires_app.is_none_or(&has_app))
        .filter_map(|(order, c)| {
            let (score, ranges) = matcher::score_with_synonyms(&c.target, &c.synonyms, &q)?;
            // The band is for "the user typed this command's NAME". A synonym
            // hit is a weaker claim and stays on the shared scale — otherwise
            // `documents` would put File Search above the user's Documents
            // folder, and `paste` above a file called paste.txt.
            let direct = matcher::score(&c.target, &q).map_or(0.0, |(s, _)| s);
            let score = if direct >= c.band_from {
                score + TIER_BONUS
            } else {
                score
            };
            Some((score, order as u32, c, ranges))
        })
        .collect();
    // Score, then declaration order: the pool is tiny and never truncates a
    // tie in practice, but the frontend breaks the same tie the same way, and
    // two sides that disagree about a sort are a bug waiting for a sixth row.
    hits.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    hits.truncate(max);
    hits.into_iter()
        .map(|(score, order, c, match_ranges)| CommandMatch {
            id: c.id.to_string(),
            name: c.name.to_string(),
            subtitle: c.subtitle,
            score,
            match_ranges,
            order,
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
    /// Every app this machine could have. Tests that care pass their own.
    fn any(_: &str) -> bool {
        true
    }

    #[test]
    fn a_builtin_outranks_the_deepest_exact_file_match() {
        let hits = match_query(&all(), "settings", MAX_RESULTS, any);
        assert!(
            hits[0].score > 1.0,
            "a built-in scored {} — a file named Settings reaches ~0.91",
            hits[0].score
        );
    }

    /// What the user asked for: typing "settings" surfaces the two settings
    /// destinations, not the filesystem's opinion of the word — and in the
    /// order they were asked for, "YSpot settings, Windows Settings", which
    /// is the declaration order. Both are WORD_START 0.8 plus the band; a
    /// name sort put Windows first on the strength of a W.
    #[test]
    fn both_settings_destinations_are_on_top_for_settings() {
        let hits = match_query(&all(), "settings", MAX_RESULTS, any);
        let names: Vec<&str> = hits.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(&names[..2], &["YSpot Settings", "Windows Settings"]);
        assert!(
            hits[0].order < hits[1].order,
            "ties follow declaration order"
        );
    }

    /// The row the user asked to sit "right under Windows Settings". Both
    /// prefix-match `windows` at 0.9 and take the band, so they tie at 1.4,
    /// and declaration order — not the alphabet — puts Settings first.
    #[test]
    fn windows_backup_sits_directly_under_windows_settings_for_windows() {
        let hits = match_query(&all(), "windows", MAX_RESULTS, any);
        let names: Vec<&str> = hits.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["Windows Settings", "Windows Backup"]);
        assert!(hits.iter().all(|h| h.score >= BAND_FLOOR), "{hits:?}");
        assert_eq!(
            hits[0].score, hits[1].score,
            "an exact tie, broken by order"
        );
    }

    /// An app's built-in takes the band from PREFIX only. `ba` word-starts
    /// "Windows Backup", and with the ordinary WORD_START rule two letters
    /// opened it from row 1, above Background and Battle.net — the accident
    /// the module doc says none of these is reachable by. `backup` still
    /// finds it, on the shared scale, where "Backup and Restore" also lives.
    #[test]
    fn an_app_builtin_is_not_promoted_from_a_word_start() {
        for q in ["ba", "back", "backup"] {
            let hits = match_query(&all(), q, MAX_RESULTS, any);
            let hit = hits
                .iter()
                .find(|h| h.id == "windows.backup")
                .unwrap_or_else(|| panic!("{q}: Windows Backup should still be findable"));
            assert!(
                hit.score < BAND_FLOOR,
                "{q}: reached the band at {}",
                hit.score
            );
        }
        // From the start of the name it is the band, as for `windows`.
        let hits = match_query(&all(), "wind", MAX_RESULTS, any);
        assert!(hits
            .iter()
            .any(|h| h.id == "windows.backup" && h.score >= BAND_FLOOR));
    }

    /// The app is Windows 11 22H2+, and the spec supports Windows 10. A row
    /// for an app the machine does not have is a top row whose Enter fails —
    /// worse than no row — so the command exists only while the app does.
    #[test]
    fn the_backup_builtin_is_absent_without_the_app() {
        let hits = match_query(&all(), "windows", MAX_RESULTS, |_| false);
        let names: Vec<&str> = hits.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["Windows Settings"]);
        // And the gate asks about the right AUMID, not just "any app".
        let asked = std::cell::RefCell::new(Vec::new());
        match_query(&all(), "windows", MAX_RESULTS, |a| {
            asked.borrow_mut().push(a.to_string());
            true
        });
        assert_eq!(asked.into_inner(), [crate::apps::WINDOWS_BACKUP_AUMID]);
    }

    /// The guard that keeps the band out of reach of a substring or fuzzy
    /// accident. "lipboard" still finds Clipboard History, on the shared
    /// scale, where it belongs.
    #[test]
    fn the_promotion_stops_below_word_start() {
        let hits = match_query(&all(), "lipboard", MAX_RESULTS, any);
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
        let hits = match_query(&all(), "documents", MAX_RESULTS, any);
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
            match_query(&cmds, q, MAX_RESULTS, any)
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
        assert!(match_query(&cmds, "s", MAX_RESULTS, any).is_empty());
        assert!(match_query(&cmds, "", MAX_RESULTS, any).is_empty());
        assert!(match_query(&cmds, "zzqxjv", MAX_RESULTS, any).is_empty());
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
            let hits = match_query(&cmds, c.name, MAX_RESULTS, any);
            assert!(
                hits.iter().any(|h| h.id == c.id),
                "{} does not match its own name",
                c.id
            );
        }
    }
}
