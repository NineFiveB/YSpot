//! Per-user settings (SPEC.md §5.9): JSON at
//! `%LOCALAPPDATA%\YSpot\settings.json`.
//!
//! Machine-local by design — hotkey bindings and index configuration MUST
//! NOT roam (§5.9), which is why this is `LOCALAPPDATA` and not `APPDATA`.
//!
//! Unknown keys are kept and written back. A settings file that a newer
//! build wrote must survive being opened by an older one, and silently
//! dropping a field the user set is the worst way to fail at that.

use std::path::PathBuf;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

/// The chord that summons the launcher (§5.1), in the shell's own terms so
/// the file stays readable and the frontend can round-trip it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hotkey {
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub shift: bool,
    #[serde(default)]
    pub win: bool,
    /// The key's name in the frontend's `KeyboardEvent.code` vocabulary,
    /// e.g. `Space`, `KeyK`, `F4`. That is what a key-capture field gives,
    /// and it is layout-independent, which a character would not be.
    pub code: String,
}

impl Default for Hotkey {
    /// §5.1: Alt+Space, matching PowerToys Run and Raycast muscle memory.
    fn default() -> Self {
        Hotkey {
            ctrl: false,
            alt: true,
            shift: false,
            win: false,
            code: "Space".to_string(),
        }
    }
}

impl Hotkey {
    /// The accelerator string Tauri's global-shortcut plugin parses, which
    /// on Windows compiles down to `RegisterHotKey` (§5.1 permits the plugin
    /// only on that condition).
    pub fn accelerator(&self) -> String {
        let mut parts: Vec<&str> = Vec::with_capacity(5);
        if self.ctrl {
            parts.push("Control");
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.shift {
            parts.push("Shift");
        }
        if self.win {
            parts.push("Super");
        }
        parts.push(&self.code);
        parts.join("+")
    }

    /// Whether this chord is one the shell will refuse to bind (§5.1): F12
    /// is reserved for debuggers, and a chord with no modifier would swallow
    /// an ordinary keypress system-wide.
    pub fn rejection(&self) -> Option<&'static str> {
        if self.code.is_empty() {
            return Some("no key");
        }
        if self.code == "F12" {
            return Some("F12 is reserved for debuggers");
        }
        if !(self.ctrl || self.alt || self.shift || self.win) {
            return Some("a hotkey needs at least one modifier");
        }
        // A lone modifier as the key is not a chord.
        if matches!(
            self.code.as_str(),
            "ControlLeft"
                | "ControlRight"
                | "AltLeft"
                | "AltRight"
                | "ShiftLeft"
                | "ShiftRight"
                | "MetaLeft"
                | "MetaRight"
        ) {
            return Some("a modifier cannot be the key");
        }
        None
    }

    /// A caution that does not block (§5.1 SHOULDs a warning on Win chords,
    /// since the OS reserves many of them).
    pub fn warning(&self) -> Option<&'static str> {
        self.win
            .then_some("Windows reserves most Win-key chords; this may not register")
    }

    /// The likely owner of a chord that failed to register (§5.1's built-in
    /// table), so the conflict message names something actionable instead of
    /// reporting an error code.
    pub fn likely_owner(&self) -> Option<&'static str> {
        match (
            self.ctrl,
            self.alt,
            self.shift,
            self.win,
            self.code.as_str(),
        ) {
            (false, true, false, false, "Space") => {
                Some("PowerToys Run, or Copilot on some Windows 11 builds")
            }
            (false, true, false, true, "Space") => Some("PowerToys Command Palette"),
            (false, false, false, true, "Space") => Some("the Windows input-language switcher"),
            _ => None,
        }
    }
}

/// Theme override (§5.8): the system's choice unless the user says otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Settings {
    #[serde(default)]
    pub hotkey: Hotkey,
    #[serde(default)]
    pub theme: Theme,
    /// Everything this build does not know about, kept so a newer build's
    /// settings survive a round trip through an older one.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

pub struct SettingsStore {
    path: Option<PathBuf>,
    current: RwLock<Settings>,
}

impl SettingsStore {
    pub fn open() -> SettingsStore {
        let path = std::env::var_os("LOCALAPPDATA")
            .map(|b| PathBuf::from(b).join("YSpot").join("settings.json"));
        match &path {
            Some(p) => Self::open_at(p.clone()),
            None => {
                log::error!("settings: LOCALAPPDATA unset; defaults only, nothing will persist");
                SettingsStore {
                    path: None,
                    current: RwLock::new(Settings::default()),
                }
            }
        }
    }

    pub fn open_at(path: PathBuf) -> SettingsStore {
        let current = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Settings>(&text) {
                Ok(s) => {
                    log::info!("settings loaded from {}", path.display());
                    s
                }
                Err(e) => {
                    // Keep the unreadable file rather than overwriting it:
                    // it is the user's, and a parse bug on our side must not
                    // destroy their bindings.
                    log::error!(
                        "settings at {} did not parse ({e}); using defaults and leaving the \
                         file alone until something is saved",
                        path.display()
                    );
                    Settings::default()
                }
            },
            Err(_) => Settings::default(),
        };
        SettingsStore {
            path: Some(path),
            current: RwLock::new(current),
        }
    }

    pub fn get(&self) -> Settings {
        self.current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Replace the settings and write them out. The in-memory copy updates
    /// even when the write fails, so the running session honors what the
    /// user just chose.
    pub fn save(&self, next: Settings) -> Result<(), String> {
        *self.current.write().unwrap_or_else(|e| e.into_inner()) = next.clone();
        let Some(path) = &self.path else {
            return Err("no settings location (LOCALAPPDATA unset)".to_string());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        }
        let text = serde_json::to_string_pretty(&next).map_err(|e| e.to_string())?;
        // Write-then-rename: a crash mid-write must not leave a truncated
        // file where the hotkey binding used to be.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text.as_bytes())
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("rename into {}: {e}", path.display()))?;
        log::info!("settings saved to {}", path.display());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("yspot-settings-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("settings.json")
    }

    #[test]
    fn the_default_is_alt_space() {
        let h = Hotkey::default();
        assert_eq!(h.accelerator(), "Alt+Space");
        assert!(h.rejection().is_none());
        assert!(h.warning().is_none());
        assert!(h.likely_owner().is_some(), "the default chord is contested");
    }

    #[test]
    fn accelerators_name_every_modifier_in_a_fixed_order() {
        let h = Hotkey {
            ctrl: true,
            alt: true,
            shift: true,
            win: true,
            code: "KeyK".into(),
        };
        assert_eq!(h.accelerator(), "Control+Alt+Shift+Super+KeyK");
        assert_eq!(
            Hotkey {
                ctrl: true,
                alt: false,
                shift: false,
                win: false,
                code: "Space".into()
            }
            .accelerator(),
            "Control+Space"
        );
    }

    #[test]
    fn unbindable_chords_are_refused_with_a_reason() {
        let base = Hotkey::default();
        let with = |f: fn(&mut Hotkey)| {
            let mut h = base.clone();
            f(&mut h);
            h
        };
        assert!(with(|h| h.code = "F12".into()).rejection().is_some());
        assert!(with(|h| h.code = String::new()).rejection().is_some());
        assert!(with(|h| h.alt = false).rejection().is_some(), "no modifier");
        assert!(with(|h| h.code = "AltLeft".into()).rejection().is_some());
        // A Win chord binds, but warns.
        let win = with(|h| h.win = true);
        assert!(win.rejection().is_none());
        assert!(win.warning().is_some());
    }

    #[test]
    fn settings_round_trip_through_the_file() {
        let path = temp_path("roundtrip");
        let store = SettingsStore::open_at(path.clone());
        assert_eq!(store.get(), Settings::default());
        let next = Settings {
            hotkey: Hotkey {
                ctrl: true,
                alt: false,
                shift: true,
                win: false,
                code: "KeyK".into(),
            },
            theme: Theme::Dark,
            ..Default::default()
        };
        store.save(next.clone()).expect("save");
        // A fresh store sees exactly what was written.
        let reopened = SettingsStore::open_at(path.clone());
        assert_eq!(reopened.get(), next);
        assert_eq!(reopened.get().hotkey.accelerator(), "Control+Shift+KeyK");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn unknown_keys_survive_a_round_trip() {
        let path = temp_path("unknown");
        std::fs::write(
            &path,
            r#"{"hotkey":{"alt":true,"code":"Space"},"futureThing":{"a":1}}"#,
        )
        .unwrap();
        let store = SettingsStore::open_at(path.clone());
        let mut s = store.get();
        assert!(s.extra.contains_key("futureThing"), "unknown key dropped");
        s.theme = Theme::Light;
        store.save(s).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("futureThing"),
            "unknown key lost on save: {text}"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn an_unparseable_file_falls_back_without_destroying_it() {
        let path = temp_path("broken");
        std::fs::write(&path, "{ not json").unwrap();
        let store = SettingsStore::open_at(path.clone());
        assert_eq!(store.get(), Settings::default());
        // The file is still there, untouched, until something is saved.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_missing_file_is_defaults_not_an_error() {
        let path = temp_path("missing").with_file_name("does-not-exist.json");
        let store = SettingsStore::open_at(path.clone());
        assert_eq!(store.get(), Settings::default());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
