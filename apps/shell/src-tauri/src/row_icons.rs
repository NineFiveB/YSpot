//! Which shell item a result row's icon comes from (SPEC.md §7.1, §7.2).
//!
//! **The parsing name is built here, in Rust, and never accepted from the
//! webview.** `SHCreateItemFromParsingName` resolves far more than app
//! identifiers — `mailto:`, `http://`, `shell:ControlPanelFolder`, arbitrary
//! namespace objects — and activating one can load an in-process shell
//! extension. Until now the hardcoded `shell:AppsFolder\` prefix was what
//! confined that surface; generalising the extractor moves the confinement
//! here, so the IPC carries `(kind, id)` — the same pair `execute_action`
//! already takes — and every string that reaches the shell is produced from a
//! catalog this process owns.
//!
//! Kept out of `lib.rs` so the mapping is unit-testable without a desktop.

use crate::apps::AppCatalog;
use crate::control_panel;
use crate::settings_catalog::{Launch, SettingsCatalog};

/// Every Settings page and the Settings home share one icon.
///
/// Verified byte-identical on Windows 11 26200 for `ms-settings:`,
/// `ms-settings:display`, `ms-settings:bluetooth` — and for a page name that
/// does not exist, which is the tell: the shell resolves the *package*, not
/// the page. Per-page pictograms live in the Settings package's resource
/// index behind an undocumented page-to-codepoint table, with no API. So all
/// 73 rows share one extraction, one disk file and one memory entry rather
/// than writing 73 identical PNGs.
pub const SETTINGS_HOME: &str = "ms-settings:";

/// The parsing name for a row, or `None` when the row legitimately has no
/// Windows icon and should keep its stroke glyph.
///
/// `Err` means the row claimed an icon that should have resolved and did not,
/// which is a bug rather than a missing pictogram.
pub fn resolve(
    kind: &str,
    id: &str,
    apps: &AppCatalog,
    settings: &SettingsCatalog,
) -> Result<Option<String>, String> {
    match kind {
        // Equality against the catalog the row itself came from, not a
        // character whitelist: Win32 AUMIDs legitimately contain backslashes,
        // braces and `!`, so a whitelist would be fragile where this is
        // exact. The only way to lose an icon here is a catalog refresh
        // between the search and the row mounting, which falls back to the
        // glyph.
        "app" => apps
            .snapshot()
            .iter()
            .any(|e| e.aumid == id)
            .then(|| format!("shell:AppsFolder\\{id}"))
            .map(Some)
            .ok_or_else(|| format!("unknown app {id}")),
        "command" => command_parsing_name(id),
        "setting" => {
            let e = settings
                .find(id)
                .ok_or_else(|| format!("unknown settings entry {id}"))?;
            Ok(setting_parsing_name(&e.launch, control_panel::clsid))
        }
        other => Err(format!("no icons for kind {other}")),
    }
}

/// The command half. Pure, so it can be checked against `commands::all()`.
pub fn command_parsing_name(id: &str) -> Result<Option<String>, String> {
    match id {
        "windows.settings" => Ok(Some(SETTINGS_HOME.to_string())),
        // The command opens this AUMID, so it wears what the app's own row
        // wore. Built here from the same constant the launch uses — never
        // from anything the webview sent — so the two cannot drift.
        "windows.backup" => Ok(Some(format!(
            "shell:AppsFolder\\{}",
            crate::apps::WINDOWS_BACKUP_AUMID
        ))),
        // §7.6 rows that are YSpot itself. There is no Windows icon for
        // "Quit YSpot", and borrowing one would be a claim the code cannot
        // back — these keep their glyph deliberately.
        "yspot.settings" | "yspot.clipboard" | "yspot.files" | "yspot.quit" => Ok(None),
        other => Err(format!("unknown command {other}")),
    }
}

/// The settings half. The CLSID lookup is injected so this is testable
/// without touching the registry.
pub fn setting_parsing_name(
    launch: &Launch,
    clsid: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    match launch {
        Launch::Uri(_) => Some(SETTINGS_HOME.to_string()),
        Launch::ControlPanel(canonical) => match clsid(canonical) {
            Some(c) => Some(format!("shell:::{c}")),
            // A canonical name this machine does not register. Not an error:
            // Windows builds and SKUs differ, and the row keeps its glyph.
            None => {
                log::debug!("no Control Panel CLSID for {canonical}; row keeps its glyph");
                None
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every built-in command must have an answer here. A command added to
    /// `commands::all()` without one would return `Err` at icon time and log
    /// a resolution failure for a row that is working perfectly well.
    #[test]
    fn every_builtin_command_is_accounted_for() {
        for c in crate::commands::all() {
            command_parsing_name(c.id).unwrap_or_else(|e| panic!("{}: {e}", c.id));
        }
    }

    /// The two commands that are Windows destinations get Windows icons —
    /// the Settings home its own, Windows Backup the one its suppressed app
    /// row wore, built from the same AUMID the launch uses; the four that
    /// are YSpot itself deliberately get none.
    #[test]
    fn only_the_windows_destinations_borrow_windows_icons() {
        assert_eq!(
            command_parsing_name("windows.settings"),
            Ok(Some(SETTINGS_HOME.to_string()))
        );
        assert_eq!(
            command_parsing_name("windows.backup"),
            Ok(Some(format!(
                "shell:AppsFolder\\{}",
                crate::apps::WINDOWS_BACKUP_AUMID
            )))
        );
        for id in [
            "yspot.settings",
            "yspot.clipboard",
            "yspot.files",
            "yspot.quit",
        ] {
            assert_eq!(command_parsing_name(id), Ok(None), "{id}");
        }
        assert!(command_parsing_name("yspot.nonexistent").is_err());
    }

    /// All 73 pages resolve to the same parsing name, so they share one
    /// extraction rather than writing 73 identical PNGs to disk.
    #[test]
    fn every_settings_page_shares_the_one_settings_icon() {
        let n = |_: &str| -> Option<String> { panic!("a URI page must not consult the registry") };
        for uri in [
            "ms-settings:",
            "ms-settings:display",
            "ms-settings:bluetooth",
        ] {
            assert_eq!(
                setting_parsing_name(&Launch::Uri(uri.to_string()), n),
                Some(SETTINGS_HOME.to_string())
            );
        }
    }

    #[test]
    fn a_control_panel_item_uses_its_clsid_and_a_missing_one_keeps_its_glyph() {
        let known = |c: &str| {
            (c == "Microsoft.System").then(|| "{BB06C0E4-D293-4f75-8A90-CB05B6477EEE}".to_string())
        };
        assert_eq!(
            setting_parsing_name(&Launch::ControlPanel("Microsoft.System".into()), known),
            Some("shell:::{BB06C0E4-D293-4f75-8A90-CB05B6477EEE}".to_string())
        );
        // Not registered on this machine: no icon, no error, no generic
        // blank-page bitmap standing in for one.
        assert_eq!(
            setting_parsing_name(&Launch::ControlPanel("Microsoft.NotHere".into()), known),
            None
        );
    }

    /// The confinement this module exists for: a kind it does not know gets
    /// an error, never a parsing name built from whatever arrived.
    #[test]
    fn an_unknown_kind_never_produces_a_parsing_name() {
        let apps = AppCatalog::new();
        let settings = SettingsCatalog::load(None, crate::settings_catalog::probe_capabilities());
        for kind in ["file", "window", "calc", "", "../../windows"] {
            assert!(
                resolve(kind, "anything", &apps, &settings).is_err(),
                "kind {kind:?} was accepted"
            );
        }
    }
}
