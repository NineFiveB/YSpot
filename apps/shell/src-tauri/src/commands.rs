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
    target: Target,
    synonyms: Vec<Target>,
}

/// A scored command hit, in the shape the frontend row needs.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandMatch {
    pub id: String,
    pub name: String,
    pub score: f32,
    pub match_ranges: Ranges,
}

pub const MAX_RESULTS: usize = 3;

/// The catalog, built once at startup.
pub fn all() -> Vec<Command> {
    [
        (
            "yspot.settings",
            "YSpot Settings",
            &["preferences", "options", "hotkey", "configure"][..],
        ),
        (
            "yspot.clipboard",
            "Clipboard History",
            &["paste", "clipboard", "copied", "history"][..],
        ),
        ("yspot.quit", "Quit YSpot", &["exit", "close yspot"][..]),
    ]
    .into_iter()
    .map(|(id, name, synonyms)| Command {
        id,
        name,
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
            matcher::score_with_synonyms(&c.target, &c.synonyms, &q).map(|(s, r)| (s, c, r))
        })
        .collect();
    hits.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.name.cmp(b.1.name)));
    hits.truncate(max);
    hits.into_iter()
        .map(|(score, c, match_ranges)| CommandMatch {
            id: c.id.to_string(),
            name: c.name.to_string(),
            score,
            match_ranges,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
