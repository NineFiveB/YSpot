//! Windows Settings pages and Control Panel items (SPEC.md §7.2).
//!
//! The catalog is data: `resources/settings-catalog.json`, shipped beside
//! the executable so it can be corrected or extended without a code release
//! (§7.2). A copy is also compiled in, which is what makes the shell work
//! from a build directory, from a test, and from an install whose resource
//! file someone deleted — the embedded copy is the floor, the file on disk
//! wins when it parses.
//!
//! Availability varies by Windows version, SKU and hardware, and a
//! `ms-settings:` URI for a page this machine does not have silently opens
//! the Settings home page instead. So each entry may declare a gate, the
//! gates are evaluated once at startup, and entries that cannot apply are
//! dropped rather than offered (§7.2).
//!
//! Launching: `ShellExecuteEx` on the URI for Settings pages, and
//! `control.exe /name Microsoft.<CanonicalName>` for Control Panel items.

use std::sync::Arc;

use serde::Deserialize;

use crate::matcher::{self, Ranges, Target};

/// The compiled-in floor; the file on disk overrides it when present.
const EMBEDDED: &str = include_str!("../resources/settings-catalog.json");

#[derive(Debug, Deserialize)]
struct CatalogFile {
    #[serde(default)]
    settings: Vec<RawSetting>,
    #[serde(default)]
    control_panel: Vec<RawControlPanel>,
}

#[derive(Debug, Deserialize)]
struct RawSetting {
    id: String,
    name: String,
    uri: String,
    #[serde(default)]
    synonyms: Vec<String>,
    #[serde(default)]
    gate: Option<Gate>,
}

#[derive(Debug, Deserialize)]
struct RawControlPanel {
    id: String,
    name: String,
    canonical: String,
    #[serde(default)]
    synonyms: Vec<String>,
    #[serde(default)]
    gate: Option<Gate>,
}

/// What a machine must have for an entry to be worth offering (§7.2).
#[derive(Debug, Deserialize, Clone)]
struct Gate {
    #[serde(default)]
    min_build: Option<u32>,
    #[serde(default)]
    requires: Option<String>,
}

/// How to open an entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    /// A `ms-settings:` URI.
    Uri(String),
    /// A Control Panel canonical name, opened through `control.exe /name`.
    ControlPanel(String),
}

#[derive(Debug, Clone)]
pub struct SettingEntry {
    pub id: String,
    pub name: String,
    pub launch: Launch,
    target: Target,
    synonyms: Vec<Target>,
}

/// A scored settings hit, in the shape the frontend row needs.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingMatch {
    pub id: String,
    pub name: String,
    /// "Settings" or "Control Panel", shown as the row's subtitle.
    pub group: &'static str,
    pub score: f32,
    pub match_ranges: Ranges,
}

/// Most settings rows a root query shows; the point is to answer, not to
/// bury the file results under a catalog dump.
pub const MAX_RESULTS: usize = 4;

/// What this machine can actually offer, decided once at startup.
#[derive(Debug, Clone, Copy)]
pub struct Capabilities {
    pub build: u32,
    pub battery: bool,
    pub cellular: bool,
    pub bluetooth: bool,
    pub pen: bool,
    pub touchpad: bool,
}

impl Capabilities {
    fn allows(&self, gate: &Option<Gate>) -> bool {
        let Some(g) = gate else { return true };
        if let Some(min) = g.min_build {
            if self.build < min {
                return false;
            }
        }
        match g.requires.as_deref() {
            None => true,
            Some("battery") => self.battery,
            Some("cellular") => self.cellular,
            Some("bluetooth") => self.bluetooth,
            Some("pen") => self.pen,
            Some("touchpad") => self.touchpad,
            // An unknown requirement is a catalog written for a newer shell.
            // Offering the entry anyway would launch a page this build cannot
            // reason about, so it stays hidden and says why once.
            Some(other) => {
                log::debug!("settings catalog: unknown gate requirement {other:?}; entry hidden");
                false
            }
        }
    }
}

pub struct SettingsCatalog {
    entries: Vec<SettingEntry>,
}

impl SettingsCatalog {
    /// Load the catalog, preferring `dir/settings-catalog.json` and falling
    /// back to the embedded copy, then drop everything this machine gates
    /// out.
    pub fn load(dir: Option<&std::path::Path>, caps: Capabilities) -> Arc<SettingsCatalog> {
        let from_disk =
            dir.map(|d| d.join("settings-catalog.json")).and_then(
                |p| match std::fs::read_to_string(&p) {
                    Ok(text) => match serde_json::from_str::<CatalogFile>(&text) {
                        Ok(c) => Some(c),
                        Err(e) => {
                            log::warn!(
                            "settings catalog at {} did not parse ({e}); using the built-in copy",
                            p.display()
                        );
                            None
                        }
                    },
                    Err(_) => None,
                },
            );
        let file = from_disk.unwrap_or_else(|| {
            serde_json::from_str(EMBEDDED).expect("the embedded catalog must parse")
        });

        let mut entries = Vec::with_capacity(file.settings.len() + file.control_panel.len());
        for s in file.settings {
            if !caps.allows(&s.gate) {
                continue;
            }
            entries.push(SettingEntry {
                target: Target::new(&s.name),
                synonyms: s.synonyms.iter().map(|t| Target::new(t)).collect(),
                id: s.id,
                name: s.name,
                launch: Launch::Uri(s.uri),
            });
        }
        for c in file.control_panel {
            if !caps.allows(&c.gate) {
                continue;
            }
            entries.push(SettingEntry {
                target: Target::new(&c.name),
                synonyms: c.synonyms.iter().map(|t| Target::new(t)).collect(),
                id: c.id,
                name: c.name,
                launch: Launch::ControlPanel(c.canonical),
            });
        }
        log::info!("settings catalog: {} entries after gating", entries.len());
        Arc::new(SettingsCatalog { entries })
    }

    pub fn find(&self, id: &str) -> Option<&SettingEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Best `max` entries for a query, by score then name.
    pub fn match_query(&self, query: &str, max: usize) -> Vec<SettingMatch> {
        let q = matcher::fold_query(query);
        // A single character matches half the catalog and says nothing; the
        // file index is the better answer for one keystroke.
        if q.len() < 2 || max == 0 {
            return Vec::new();
        }
        let mut hits: Vec<(f32, &SettingEntry, Ranges)> = self
            .entries
            .iter()
            .filter_map(|e| {
                matcher::score_with_synonyms(&e.target, &e.synonyms, &q).map(|(s, r)| (s, e, r))
            })
            .collect();
        hits.sort_by(|a, b| {
            b.0.total_cmp(&a.0)
                .then_with(|| a.1.name.len().cmp(&b.1.name.len()))
                .then_with(|| a.1.name.cmp(&b.1.name))
        });
        hits.truncate(max);
        hits.into_iter()
            .map(|(score, e, match_ranges)| SettingMatch {
                id: e.id.clone(),
                name: e.name.clone(),
                group: match e.launch {
                    Launch::Uri(_) => "Settings",
                    Launch::ControlPanel(_) => "Control Panel",
                },
                score,
                match_ranges,
            })
            .collect()
    }
}

/// Open a catalog entry (§7.2).
pub fn launch(entry: &SettingEntry) -> Result<(), String> {
    match &entry.launch {
        Launch::Uri(uri) => crate::shell_open(uri),
        Launch::ControlPanel(canonical) => {
            // `control.exe /name <canonical>` is the documented way in, and
            // going through the shell rather than CreateProcess keeps this on
            // the same unelevated, default-verb path as everything else.
            crate::shell_execute("control.exe", &format!("/name {canonical}"))
        }
    }
}

// ---------------------------------------------------------------------------
// Capability probing

pub fn probe_capabilities() -> Capabilities {
    Capabilities {
        build: windows_build(),
        battery: has_battery(),
        // Radio and adapter probing needs WinRT or SetupAPI enumeration,
        // which is a dependency for a gate that hides at most a handful of
        // rows. Until that lands, these read as present: an offered page
        // that opens the Settings home is a smaller failure than a page the
        // machine has and cannot find. Recorded in docs/M1.md.
        cellular: true,
        bluetooth: true,
        pen: true,
        touchpad: true,
    }
}

/// Windows build number from the registry — `GetVersionEx` lies to
/// unmanifested processes, and `RtlGetVersion` needs ntdll.
fn windows_build() -> u32 {
    use windows::core::w;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegGetValueW, HKEY, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ,
    };
    let mut buf = [0u16; 32];
    let mut size = std::mem::size_of_val(&buf) as u32;
    // SAFETY: static key and value names; `buf`/`size` describe one buffer.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"),
            w!("CurrentBuildNumber"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    let _ = HKEY::default();
    let _: unsafe fn(HKEY) -> _ = RegCloseKey; // no key opened; nothing to close
    if rc.is_err() {
        return 0;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n]).parse().unwrap_or(0)
}

fn has_battery() -> bool {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    let mut status = SYSTEM_POWER_STATUS::default();
    // SAFETY: plain out-parameter call over a POD struct.
    if unsafe { GetSystemPowerStatus(&mut status) }.is_err() {
        return false;
    }
    // 128 means "no system battery"; 255 means unknown, which on a desktop
    // is the usual answer and is not evidence of a battery.
    status.BatteryFlag != 128 && status.BatteryFlag != 255
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps() -> Capabilities {
        Capabilities {
            build: 26100,
            battery: false,
            cellular: false,
            bluetooth: true,
            pen: false,
            touchpad: false,
        }
    }

    fn catalog() -> Arc<SettingsCatalog> {
        SettingsCatalog::load(None, caps())
    }

    /// The Settings HOME moved into `commands.rs` (§7.2 amendment). While it
    /// lived here too, typing "settings" produced two rows opening the same
    /// destination — one from this catalog and one from the AppsFolder.
    /// Every Control Panel canonical name in the catalog must be one this
    /// machine actually registers.
    ///
    /// `control.exe /name <canonical>` is the launch contract, so a name
    /// Windows does not know is a row that opens nothing — and it fails
    /// silently, because the launch is a fire-and-forget shell execute. The
    /// catalog shipped `Microsoft.TaskbarAndStartMenu`, which does not exist;
    /// the registered name is `Microsoft.Taskbar`.
    ///
    /// This reads the live registry, so it is machine-dependent by design:
    /// a name absent on a future Windows should fail here rather than in a
    /// user's hands.
    #[test]
    fn every_control_panel_canonical_name_is_registered_on_this_machine() {
        let c = catalog();
        let mut missing = Vec::new();
        for e in &c.entries {
            if let Launch::ControlPanel(canonical) = &e.launch {
                if crate::control_panel::clsid(canonical).is_none() {
                    missing.push(format!("{} ({canonical})", e.id));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "these rows name a Control Panel item Windows does not register,              so they open nothing: {missing:?}"
        );
    }

    #[test]
    fn the_settings_home_is_not_in_the_data_catalog() {
        let c = catalog();
        assert!(
            c.find("ms-settings:").is_none(),
            "the Settings home is a built-in command now; a catalog entry for              it is a duplicate row"
        );
    }

    #[test]
    fn the_embedded_catalog_parses_and_is_substantial() {
        let c = SettingsCatalog::load(
            None,
            Capabilities {
                build: 99999,
                battery: true,
                cellular: true,
                bluetooth: true,
                pen: true,
                touchpad: true,
            },
        );
        // Enough to be worth having; the exact count will drift as the file
        // is edited, which is the point of it being data.
        assert!(c.entries.len() > 60, "only {} entries", c.entries.len());
        // Both kinds are present and launch differently.
        assert!(matches!(
            c.find("ms-settings:display").map(|e| &e.launch),
            Some(Launch::Uri(_))
        ));
        assert!(matches!(
            c.find("cpl:devicemanager").map(|e| &e.launch),
            Some(Launch::ControlPanel(_))
        ));
    }

    /// The catalog is data meant to be edited without a code release, so the
    /// shapes a reader relies on are asserted here rather than discovered by
    /// a user whose query silently stops matching.
    #[test]
    fn the_catalog_data_is_well_formed() {
        let file: CatalogFile = serde_json::from_str(EMBEDDED).expect("parses");
        let mut ids: Vec<&str> = file
            .settings
            .iter()
            .map(|e| e.id.as_str())
            .chain(file.control_panel.iter().map(|e| e.id.as_str()))
            .collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "duplicate ids in the catalog");

        for e in &file.settings {
            assert!(
                e.uri.starts_with("ms-settings:"),
                "{} has a non-settings uri {:?}",
                e.id,
                e.uri
            );
            assert!(!e.name.is_empty(), "{} has no name", e.id);
        }
        for e in &file.control_panel {
            // `control.exe /name` takes a canonical name, and every documented
            // one is `Microsoft.<Something>`.
            assert!(
                e.canonical.starts_with("Microsoft."),
                "{} has a non-canonical name {:?}",
                e.id,
                e.canonical
            );
        }
        // Gates must name a requirement this build can evaluate, or the entry
        // silently disappears for everyone.
        for gate in file
            .settings
            .iter()
            .filter_map(|e| e.gate.as_ref())
            .chain(file.control_panel.iter().filter_map(|e| e.gate.as_ref()))
        {
            if let Some(req) = gate.requires.as_deref() {
                assert!(
                    matches!(
                        req,
                        "battery" | "cellular" | "bluetooth" | "pen" | "touchpad"
                    ),
                    "unknown gate requirement {req:?} in the shipped catalog"
                );
            }
        }
    }

    #[test]
    fn gates_hide_what_the_machine_cannot_do() {
        let c = catalog();
        // No battery, no cellular, no pen, no touchpad on this fake machine.
        assert!(c.find("ms-settings:batterysaver").is_none());
        assert!(c.find("ms-settings:network-cellular").is_none());
        assert!(c.find("ms-settings:pen").is_none());
        assert!(c.find("ms-settings:devices-touchpad").is_none());
        // Bluetooth is present, so its page stays.
        assert!(c.find("ms-settings:bluetooth").is_some());
        // Ungated entries are always there.
        assert!(c.find("ms-settings:display").is_some());
    }

    #[test]
    fn min_build_gates_on_the_version() {
        let json = r#"{"settings":[
            {"id":"a","name":"Old","uri":"ms-settings:a","gate":{"min_build":10000}},
            {"id":"b","name":"New","uri":"ms-settings:b","gate":{"min_build":99999}}
        ]}"#;
        let dir = std::env::temp_dir().join(format!("yspot-cat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("settings-catalog.json"), json).unwrap();
        let c = SettingsCatalog::load(Some(&dir), caps());
        assert!(c.find("a").is_some(), "build 26100 >= 10000");
        assert!(c.find("b").is_none(), "build 26100 < 99999");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unknown_requirement_hides_the_entry_rather_than_launching_it() {
        let json = r#"{"settings":[
            {"id":"x","name":"Future","uri":"ms-settings:x","gate":{"requires":"quantum"}}
        ]}"#;
        let dir = std::env::temp_dir().join(format!("yspot-cat2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("settings-catalog.json"), json).unwrap();
        assert!(SettingsCatalog::load(Some(&dir), caps())
            .find("x")
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_file_falls_back_to_the_embedded_copy() {
        let dir = std::env::temp_dir().join(format!("yspot-cat3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("settings-catalog.json"), "{ not json").unwrap();
        let c = SettingsCatalog::load(Some(&dir), caps());
        assert!(c.entries.len() > 60, "fell back to nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn queries_find_pages_by_name_and_by_synonym() {
        let c = catalog();
        let names = |q: &str| -> Vec<String> {
            c.match_query(q, MAX_RESULTS)
                .into_iter()
                .map(|m| m.name)
                .collect()
        };
        assert_eq!(names("display")[0], "Display");
        // Synonyms are how someone finds a page whose name they do not know.
        assert!(names("wallpaper").contains(&"Background".to_string()));
        assert!(names("dark mode").contains(&"Colors".to_string()));
        assert!(names("uninstall")
            .iter()
            .any(|n| n.contains("Installed apps") || n.contains("Programs and Features")));
        assert!(names("environment variables").contains(&"System Properties".to_string()));
        // A one-character query is not a settings question.
        assert!(c.match_query("d", MAX_RESULTS).is_empty());
        assert!(c.match_query("", MAX_RESULTS).is_empty());
        // Nonsense finds nothing.
        assert!(c.match_query("zzqxjv", MAX_RESULTS).is_empty());
    }

    #[test]
    fn results_are_capped_and_ordered_by_score() {
        let c = catalog();
        let hits = c.match_query("net", MAX_RESULTS);
        assert!(hits.len() <= MAX_RESULTS);
        for w in hits.windows(2) {
            assert!(w[0].score >= w[1].score, "{:?}", hits);
        }
        // The group label is what the row shows as its subtitle.
        assert!(hits
            .iter()
            .all(|h| h.group == "Settings" || h.group == "Control Panel"));
    }

    #[test]
    fn probing_this_machine_answers_something_sane() {
        let c = probe_capabilities();
        assert!(c.build >= 10000, "build {} looks wrong", c.build);
    }
}
