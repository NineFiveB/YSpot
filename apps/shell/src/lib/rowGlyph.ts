// Which Fluent glyph a row wears, when it has no icon of its own.
//
// The problem this solves is in row_icons.rs: `SHCreateItemFromParsingName`
// resolves the Settings PACKAGE, not the page, so all 73 `ms-settings:` rows
// came back byte-identical. Windows Security, Bluetooth and Display were the
// same picture. Control Panel items are different — each has its own CLSID
// and a genuinely distinct bitmap — so those keep their extracted icon and
// the key here is only a fallback for a CLSID this machine does not register.
//
// The keys come from the vendored set, which was chosen for exactly this
// table; the table itself was lost and reconstructed, then audited for
// coverage, collisions and wrong metaphors. No two `ms-settings:` pages share
// a key, which is the property that matters — `glyphKeysAreDistinct` in the
// test file holds it.

import type { Row } from "./ipc";
import { FLUENT_20, FLUENT_VIEWBOX } from "./fluentGlyphs";
import { YSPOT_20 } from "./yspotGlyphs";

/** Every glyph, by key. The two sets are disjoint; a test pins that. */
export const GLYPHS: Readonly<Record<string, string>> = { ...FLUENT_20, ...YSPOT_20 };

/** Shared by both sets, so one `<svg>` serves every row. */
export const GLYPH_VIEWBOX = FLUENT_VIEWBOX;

/**
 * Settings pages and Control Panel items, by catalog id.
 *
 * A page with no entry falls back to its kind, which is a gear — correct but
 * indistinct, so the test requires every catalog id to be present.
 */
export const SETTING_GLYPH: Readonly<Record<string, string>> = {
  "ms-settings:display":                    "desktop", // Display
  "ms-settings:nightlight":                 "weather_moon", // Night light
  "ms-settings:sound":                      "speaker_2", // Sound
  "ms-settings:apps-volume":                "speaker_settings", // Volume mixer
  "ms-settings:notifications":              "alert", // Notifications
  "ms-settings:powersleep":                 "power", // Power & sleep
  "ms-settings:batterysaver":               "battery_saver", // Battery saver
  "ms-settings:storagesense":               "hard_drive", // Storage
  "ms-settings:multitasking":               "window_multiple", // Multitasking
  "ms-settings:project":                    "desktop_signal", // Projecting to this PC
  "ms-settings:clipboard":                  "clipboard", // Clipboard
  "ms-settings:about":                      "info", // About this PC
  "ms-settings:bluetooth":                  "bluetooth", // Bluetooth & devices
  "ms-settings:connecteddevices":           "plug_connected", // Devices
  "ms-settings:printers":                   "print", // Printers & scanners
  "ms-settings:mousetouchpad":              "cursor", // Mouse
  "ms-settings:devices-touchpad":           "tap_single", // Touchpad
  "ms-settings:pen":                        "pen", // Pen & Windows Ink
  "ms-settings:typing":                     "text_field", // Typing
  "ms-settings:keyboard":                   "keyboard", // Keyboard
  "ms-settings:autoplay":                   "play_settings", // AutoPlay
  "ms-settings:usb":                        "usb_plug", // USB
  "ms-settings:network":                    "globe", // Network & internet
  "ms-settings:network-wifi":               "wifi_1", // Wi-Fi
  "ms-settings:network-ethernet":           "globe_desktop", // Ethernet
  "ms-settings:network-vpn":                "globe_shield", // VPN
  "ms-settings:network-mobilehotspot":      "wifi_settings", // Mobile hotspot
  "ms-settings:network-airplanemode":       "airplane", // Airplane mode
  "ms-settings:network-proxy":              "arrow_routing", // Proxy
  "ms-settings:network-cellular":           "sim", // Cellular
  "ms-settings:network-status":             "network_check", // Network status
  "ms-settings:personalization":            "paint_brush", // Personalization
  "ms-settings:personalization-background": "wallpaper", // Background
  "ms-settings:colors":                     "color", // Colors
  "ms-settings:themes":                     "paint_bucket", // Themes
  "ms-settings:lockscreen":                 "lock_closed", // Lock screen
  "ms-settings:taskbar":                    "panel_bottom", // Taskbar
  "ms-settings:personalization-start":      "grid", // Start
  "ms-settings:fonts":                      "text_font", // Fonts
  "ms-settings:appsfeatures":               "apps_list", // Installed apps
  "ms-settings:defaultapps":                "apps_settings", // Default apps
  "ms-settings:startupapps":                "rocket", // Startup apps
  "ms-settings:optionalfeatures":           "puzzle_piece", // Optional features
  "ms-settings:appsforwebsites":            "link", // Apps for websites
  "ms-settings:yourinfo":                   "person", // Your info
  "ms-settings:emailandaccounts":           "mail", // Email & accounts
  "ms-settings:signinoptions":              "key", // Sign-in options
  "ms-settings:otherusers":                 "people", // Other users
  "ms-settings:sync":                       "yspot_backup_restore", // Windows backup
  "ms-settings:dateandtime":                "calendar_clock", // Date & time
  "ms-settings:regionlanguage":             "local_language", // Language & region
  "ms-settings:speech":                     "person_voice", // Speech
  "ms-settings:gaming-gamebar":             "games", // Game Bar
  "ms-settings:gaming-gamemode":            "top_speed", // Game Mode
  "ms-settings:easeofaccess-display":       "text_font_size", // Accessibility: text size
  "ms-settings:easeofaccess-magnifier":     "zoom_in", // Magnifier
  "ms-settings:easeofaccess-narrator":      "read_aloud", // Narrator
  "ms-settings:easeofaccess-highcontrast":  "circle_half_fill", // Contrast themes
  "ms-settings:easeofaccess-keyboard":      "accessibility", // Accessibility: keyboard
  "ms-settings:privacy":                    "lock_shield", // Privacy & security
  "ms-settings:privacy-microphone":         "mic", // Microphone privacy
  "ms-settings:privacy-webcam":             "camera", // Camera privacy
  "ms-settings:privacy-location":           "location", // Location privacy
  "ms-settings:windowsdefender":            "yspot_windows_shield", // Windows Security
  "ms-settings:findmydevice":               "radar", // Find my device
  "ms-settings:deviceencryption":           "shield_keyhole", // Device encryption
  "ms-settings:windowsupdate":              "arrow_sync", // Windows Update
  "ms-settings:windowsupdate-history":      "history", // Update history
  "ms-settings:windowsupdate-options":      "arrow_clockwise_dashes_settings", // Update advanced options
  "ms-settings:recovery":                   "arrow_reset", // Recovery
  "ms-settings:troubleshoot":               "wrench", // Troubleshoot
  "ms-settings:developers":                 "code", // For developers
  "ms-settings:remotedesktop":              "desktop_arrow_right", // Remote Desktop

  // Control Panel. These normally show their own CLSID bitmap; the key is the
  // fallback for a machine that does not register the canonical name.
  "cpl:programs":                           "app_generic", // Programs and Features
  "cpl:devicemanager":                      "developer_board", // Device Manager
  "cpl:network":                            "phone_laptop", // Network and Sharing Center
  "cpl:power":                              "battery_charge", // Power Options
  "cpl:sound":                              "sound_wave_circle", // Sound Control Panel
  "cpl:system":                             "desktop_tower", // System Properties
  "cpl:datetime":                           "clock", // Date and Time Control Panel
  "cpl:region":                             "globe_location", // Region
  "cpl:credentials":                        "key_multiple", // Credential Manager
  "cpl:firewall":                           "shield", // Windows Defender Firewall
  "cpl:useraccounts":                       "person_settings", // User Accounts
  "cpl:mouse":                              "options", // Mouse Properties
  "cpl:fileexplorer":                       "folder", // File Explorer Options
  "cpl:defaultprograms":                    "checkmark_starburst", // Default Programs
  "cpl:backup":                             "archive", // Backup and Restore
  "cpl:recovery":                           "arrow_undo", // Recovery Control Panel
  "cpl:troubleshooting":                    "wrench_screwdriver", // Troubleshooting
  "cpl:indexing":                           "database_search", // Indexing Options
  "cpl:autoplay":                           "play_circle", // AutoPlay Control Panel
  "cpl:taskbarnav":                         "window", // Taskbar and Navigation
};

/**
 * The six built-ins (§7.6).
 *
 * `windows.settings` and `windows.backup` open Windows destinations and carry
 * real extracted icons, so theirs are fallbacks; the four YSpot ones have no
 * Windows icon to borrow and this is all they ever show.
 */
export const COMMAND_GLYPH: Readonly<Record<string, string>> = {
  "yspot.settings":   "settings",
  "windows.settings": "settings",
  "windows.backup":   "yspot_backup_restore",
  "yspot.clipboard":  "clipboard_paste",
  "yspot.files":      "folder_search",
  "yspot.quit":       "dismiss_circle",
};

/**
 * Last resort, per kind.
 *
 * `app`, `window` and the file mark reuse keys that a Control Panel row also
 * names. That is deliberate and cheap: those rows show their own bitmap, so
 * the pair is only ever visible together when a CLSID is missing.
 */
export const KIND_GLYPH_KEY: Readonly<Record<Row["kind"], string>> = {
  calc: "calculator",
  command: "window_console",
  setting: "settings",
  app: "app_generic",
  window: "window",
  file: "document",
};

/** The key a row wears. Never null: every kind has a fallback. */
export function glyphKey(row: Row): string {
  if (row.kind === "setting") return SETTING_GLYPH[row.id] ?? KIND_GLYPH_KEY.setting;
  if (row.kind === "command") return COMMAND_GLYPH[row.id] ?? KIND_GLYPH_KEY.command;
  return KIND_GLYPH_KEY[row.kind];
}

/**
 * The path data a row draws, or null if its key is somehow absent — which the
 * tests forbid, but a runtime null is a missing icon and a throw is a blank
 * launcher.
 */
export function glyphPath(row: Row): string | null {
  return GLYPHS[glyphKey(row)] ?? null;
}
