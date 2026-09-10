//! YSpot shell — Tauri v2 host (SPEC.md §5):
//! Alt+Space toggle via the global-shortcut plugin (RegisterHotKey on
//! Windows, §5.1), §5.3 placement, §5.2 focus model, indexd pipe client,
//! the §7.1 app catalog, the §7.1 per-user frecency store, and the §4.6
//! command surface.
//!
//! Still deferred: hotkey conflict dialog and rebinding UI (§5.1), the
//! Settings window (§5.9).

mod apps;
mod autostart;
mod calc;
mod clipboard;
mod com;
mod commands;
mod control_panel;
mod diagnostics;
mod etw_mark;
mod file_actions;
mod focus;
mod frecency;
mod hotkey_signal;
mod icons;
mod matcher;
mod pipe_client;
mod placement;
mod row_icons;
mod search_fallback;
mod settings;
mod settings_catalog;
mod tray;
mod winman;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use apps::AppCatalog;
use clipboard::ClipboardStore;
use frecency::Frecency;
use icons::IconCache;
use pipe_client::PipeClient;
use search_fallback::{FileSearch, WindowsSearch};
use serde::Serialize;
use settings::{Settings, SettingsStore};
use settings_catalog::SettingsCatalog;
use std::str::FromStr;
use tauri::{AppHandle, Emitter, Manager};
use yspot_proto::Filters;

use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

/// Parse a §5.1 chord into the plugin's shortcut type.
///
/// The plugin does not expose `MOD_NOREPEAT`, so holding the chord can
/// retrigger the toggle; the toggle is idempotent enough that this is a
/// cosmetic wart rather than a correctness one.
fn shortcut_of(hotkey: &settings::Hotkey) -> Option<Shortcut> {
    let mut mods = Modifiers::empty();
    if hotkey.ctrl {
        mods |= Modifiers::CONTROL;
    }
    if hotkey.alt {
        mods |= Modifiers::ALT;
    }
    if hotkey.shift {
        mods |= Modifiers::SHIFT;
    }
    if hotkey.win {
        mods |= Modifiers::SUPER;
    }
    let code = Code::from_str(&hotkey.code).ok()?;
    Some(Shortcut::new(Some(mods), code))
}

/// The last registration outcome for the bound chord: `Some(message)` while
/// the hotkey is not actually held by us.
///
/// §5.1 forbids silent degradation, and a chord that failed to register is
/// exactly that — the launcher is unreachable by the only means most people
/// will try. The static [`settings::Hotkey::rejection`] check cannot see it:
/// it validates the chord's shape, and a persisted chord has already passed
/// that. Only the registrar knows, so the registrar records it here and
/// every surface that reports on the hotkey reads it.
#[derive(Default)]
struct HotkeyState(Mutex<Option<String>>);

impl HotkeyState {
    fn get(app: &AppHandle) -> Option<String> {
        app.try_state::<HotkeyState>()
            .and_then(|s| s.0.lock().ok().and_then(|g| g.clone()))
    }
}

/// What the registrar did, and what this shell holds afterwards.
struct Rebind {
    outcome: Result<(), String>,
    /// Whether ANY chord is registered by this process after the call —
    /// the one asked for, or the one put back. This is what decides whether
    /// a refusal is a standing conflict or a non-event.
    bound: bool,
}

/// Register `next`, replacing `old` if given.
///
/// §5.1's atomicity rule: unregister the old chord, register the new one,
/// and if that fails put the old one back — a failed rebind must not leave
/// the launcher unreachable. This function only talks to the registrar; what
/// the outcome MEANS (a conflict to show, or nothing) depends on why it was
/// called, so the caller publishes that through [`set_standing_conflict`].
fn rebind_hotkey(
    app: &AppHandle,
    old: Option<&settings::Hotkey>,
    next: &settings::Hotkey,
) -> Rebind {
    let Some(shortcut) = shortcut_of(next) else {
        return Rebind {
            outcome: Err(format!("unknown key {:?}", next.code)),
            bound: old.is_some(),
        };
    };
    if let Some(reason) = next.rejection() {
        return Rebind {
            outcome: Err(reason.to_string()),
            bound: old.is_some(),
        };
    }
    let manager = app.global_shortcut();
    let previous = old.and_then(shortcut_of);
    if let Some(p) = previous {
        let _ = manager.unregister(p);
    }
    match manager.register(shortcut) {
        Ok(()) => {
            log::info!("hotkey bound to {} (§5.1)", next.accelerator());
            Rebind {
                outcome: Ok(()),
                bound: true,
            }
        }
        Err(e) => {
            // Put the old chord back before reporting, so the launcher stays
            // reachable while the user picks another one.
            let restored = previous.is_some_and(|p| manager.register(p).is_ok());
            let owner = next
                .likely_owner()
                .map(|o| format!(" It is probably held by {o}."))
                .unwrap_or_default();
            Rebind {
                outcome: Err(format!(
                    "{} could not be registered.{owner} ({e})",
                    next.accelerator()
                )),
                bound: restored,
            }
        }
    }
}

/// Give a chord back. Used when YKeys takes the hotkey over (§5.1 amended):
/// two registrations of one chord is how a launcher gets summoned twice.
fn unregister_hotkey(app: &AppHandle, chord: &settings::Hotkey) {
    if let Some(s) = shortcut_of(chord) {
        let _ = app.global_shortcut().unregister(s);
        log::info!("hotkey {} released (§5.1)", chord.accelerator());
    }
}

/// [`set_standing_conflict`], but only when there is something to say — a
/// `None` here means "leave whatever is published alone", not "clear it".
fn set_standing_conflict_if(app: &AppHandle, message: Option<String>) {
    if message.is_some() {
        set_standing_conflict(app, message);
    }
}

/// Publish §5.1's standing conflict to every surface that reports it: the
/// state onboarding and Settings read, and the tray tooltip.
fn set_standing_conflict(app: &AppHandle, message: Option<String>) {
    if let Some(state) = app.try_state::<HotkeyState>() {
        if let Ok(mut g) = state.0.lock() {
            *g = message.clone();
        }
    }
    tray::set_hotkey_conflict(app, message.as_deref());
}

#[derive(Default)]
struct WarmState(AtomicBool);

/// A view asked for before the page could listen.
///
/// `emit` reaches whatever is listening *now*. During `setup` the webview has
/// not run its `listen()` calls yet, so a view opened from the command line
/// (`--settings`), first-run onboarding, or §5.1's conflict remedy is emitted
/// into nothing: the launcher appears showing an empty query instead of the
/// thing that was asked for. [`frontend_ready`] replays it — the same
/// handshake, and the same reason, as the pipe's connection state.
#[derive(Default)]
struct PendingView(Mutex<Option<String>>);

/// Whether the launcher is showing a view opened in place (Settings) rather
/// than the results list.
///
/// Blur is a dismissal path for the results list (§5.2 step 3): you clicked
/// away, you meant to leave. It is the wrong rule for a view with controls
/// in it — a native `<select>` dropdown takes focus out of the webview, and
/// dismissing there would close Settings the moment the user tried to change
/// the theme. In a view, the launcher closes on Esc or the hotkey, not on
/// losing focus.
#[derive(Default)]
struct ViewState(AtomicBool);

#[derive(Serialize)]
struct Accepted {
    accepted: bool,
}

// ---------------------------------------------------------------------------
// Window lifecycle (§5.2 focus model).

pub(crate) fn toggle(app: &AppHandle) {
    let Some(window) = app.get_webview_window("launcher") else {
        return;
    };
    let visible = window.is_visible().unwrap_or(false);
    // Debug, not trace: when an automated M0 run wedges, whether each hotkey
    // resolved to show or dismiss is the first question every time.
    log::debug!(
        "toggle: visible={visible} -> {}",
        if visible { "dismiss" } else { "show" }
    );
    if visible {
        dismiss(app);
    } else {
        show(app);
    }
}

pub(crate) fn show(app: &AppHandle) {
    let Some(window) = app.get_webview_window("launcher") else {
        return;
    };
    // §7.1: a catalog older than five minutes is refreshed when the popup
    // shows. Asynchronous by contract — enumeration must never block the
    // results pipeline — so this show uses the current snapshot.
    if let Some(catalog) = app.try_state::<Arc<AppCatalog>>() {
        if catalog.is_stale() {
            catalog.refresh_async();
        }
    }
    // §7.5: the window list is about to be read and is most likely to have
    // changed since the last summon, so drop the cache now rather than
    // serving a window that has since closed.
    if let Some(win) = app.try_state::<Arc<winman::WindowCache>>() {
        win.invalidate();
    }
    // §5.2 step 1: record the foreground window before we take focus.
    focus::remember_foreground();
    // §5.3 places the launcher on a SUMMON. Already visible means this is a
    // raise, not a summon — the tray's "Open YSpot", a second launch, or a
    // signalled `show` — and re-placing would resize a taller in-place view
    // back to §5.3's 480 with nothing to put it right: the frontend's height
    // effect is keyed on the view, which has not changed.
    let already_visible = window.is_visible().unwrap_or(false);
    if already_visible {
        log::debug!("show: already visible; raising without re-placing");
    }
    match placement::compute_placement().filter(|_| !already_visible) {
        Some(p) => {
            // Position FIRST, then size. The placement is in the DESTINATION
            // monitor's DPI; applying it as a size while the window still sits
            // on the previous monitor means the move that follows raises
            // WM_DPICHANGED, and Windows' suggested rect scales that size
            // again by new/old — a 150% laptop to a 100% external halved the
            // launcher, and the reverse overflowed it. Moving first puts the
            // DPI change before the size that is already expressed in it.
            if let Err(e) = window.set_position(tauri::PhysicalPosition::new(p.x, p.y)) {
                log::warn!("set_position failed: {e}");
            }
            if let Err(e) = window.set_size(tauri::PhysicalSize::new(p.width, p.height)) {
                log::warn!("set_size failed: {e}");
            }
        }
        None => log::warn!("placement computation failed; keeping last position"),
    }
    // §10 M0 harness endpoint, written BEFORE the window is shown: the
    // harness takes hotkey→visible as its injected-keydown QPC to the first
    // DWM composition after this marker, and a marker placed after `show()`
    // could postdate a fast present — the compositor would already have drawn
    // the window, the harness would skip that composition, and the gated
    // number would silently ride the next unrelated one.
    etw_mark::mark("shown");
    if let Err(e) = window.show() {
        log::error!("window show failed: {e}");
        return;
    }
    let _ = window.set_focus();
    // §5.2: SetForegroundWindow(own) — succeeds because the hotkey press made
    // us the last-input process.
    match window.hwnd() {
        Ok(h) => focus::force_foreground(h.0 as isize),
        Err(e) => log::warn!("own hwnd unavailable: {e}"),
    }
    if let Err(e) = app.emit("window:shown", ()) {
        log::warn!("emit window:shown failed: {e}");
    }
    // What the webview is actually showing. One debug line per show, and it
    // is the line that caught the blank-launcher bug: a release build without
    // the `custom-protocol` feature navigates to build.devUrl
    // (localhost:5173) instead of the embedded assets, and NOTHING else in
    // the process betrays it — the window, hotkey, markers, and pipe all
    // work over a webview showing a connection error.
    match window.url() {
        Ok(u) => log::debug!("webview url: {u}"),
        Err(e) => log::debug!("webview url unavailable: {e}"),
    }
}

/// Hide the launcher and put focus back where it came from.
///
/// Returns the window focus was handed to, for the one caller that needs to
/// know: §7.4's paste synthesises Ctrl+V and must not do so if the restore was
/// refused (`clipboard::paste`).
fn dismiss(app: &AppHandle) -> Option<isize> {
    let window = app.get_webview_window("launcher")?;
    if !window.is_visible().unwrap_or(false) {
        return None;
    }
    log::debug!("dismiss: hiding");
    if let Err(e) = window.hide() {
        log::warn!("window hide failed: {e}");
    }
    // A view does not survive the window being dismissed: the next summon is
    // a fresh search, which is what the hotkey means.
    if let Some(v) = app.try_state::<ViewState>() {
        v.0.store(false, Ordering::SeqCst);
    }
    let _ = app.emit("view:reset", ());
    // §5.2 step 3: hand focus back exactly where it was.
    let restored = focus::restore_foreground();
    etw_mark::mark("hidden");
    let _ = app.emit("window:hidden", ());
    // Best effort: stop in-flight work for the current generation (§4.3 Cancel).
    if let Some(pipe) = app.try_state::<Arc<PipeClient>>() {
        let _ = pipe.cancel_current();
    }
    restored
}

// ---------------------------------------------------------------------------
// Commands (§4.6 subset).

/// One generation's shell-side answers (§5.11: catalog sources land in ~1 ms,
/// ahead of the service's first batch, in the same frame as the keystroke).
///
/// One event rather than three: everything here is computed synchronously in
/// the same command, so splitting it would only cost extra round trips and
/// give the frontend more arrival orders to reason about.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ShellResults {
    gen: u64,
    apps: Vec<apps::AppMatch>,
    settings: Vec<settings_catalog::SettingMatch>,
    commands: Vec<commands::CommandMatch>,
    windows: Vec<winman::WindowMatch>,
    calc: Option<CalcRow>,
}

/// The calculator's answer row (§7.7): shown first, Enter copies it.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct CalcRow {
    display: String,
    value: String,
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn search(
    app: AppHandle,
    pipe: tauri::State<'_, Arc<PipeClient>>,
    catalog: tauri::State<'_, Arc<AppCatalog>>,
    settings: tauri::State<'_, Arc<SettingsCatalog>>,
    builtins: tauri::State<'_, Arc<Vec<commands::Command>>>,
    win_cache: tauri::State<'_, Arc<winman::WindowCache>>,
    frec: tauri::State<'_, Arc<Frecency>>,
    gen: u64,
    text: String,
) -> Result<Accepted, String> {
    // Length, not the text. Everything typed or pasted into the query box
    // would otherwise be written to a log file that persists across restarts
    // and goes out with any support bundle — and people paste things into a
    // launcher that they would not paste into a bug report.
    log::debug!("search cmd: gen={gen} len={}", text.chars().count());
    // The shell's own answers first, and synchronously: matching a few
    // hundred names is microseconds and the calculator is a parse of one
    // line, so all of it fits the §2.5 shell-routing share and lands in the
    // same animation frame as the keystroke that asked for it.
    let mut apps = apps::match_query(
        &catalog.snapshot(),
        &text,
        apps::MAX_APP_RESULTS * FRECENCY_POOL,
    );
    for it in &mut apps {
        // §7.1: frecency reorders within a tier and can never lift a lower
        // tier above a higher one — the bonus is bounded under the tier gap.
        it.score += frec.bonus(&it.id);
    }
    apps.sort_by(|a, b| b.score.total_cmp(&a.score));
    apps.truncate(apps::MAX_APP_RESULTS);

    let mut settings = settings.match_query(&text, settings_catalog::MAX_RESULTS * FRECENCY_POOL);
    for it in &mut settings {
        it.score += frec.bonus(&it.id);
    }
    settings.sort_by(|a, b| b.score.total_cmp(&a.score));
    settings.truncate(settings_catalog::MAX_RESULTS);

    // A built-in that opens an app is offered only while that app is in the
    // catalog — the same snapshot the app rows above were matched against,
    // so the two can never disagree about what this machine has.
    let installed = catalog.snapshot();
    let mut builtin_hits = commands::match_query(
        &builtins,
        &text,
        commands::MAX_RESULTS * FRECENCY_POOL,
        |aumid| {
            installed
                .iter()
                .any(|e| e.aumid.eq_ignore_ascii_case(aumid))
        },
    );
    for it in &mut builtin_hits {
        it.score += frec.bonus(&it.id);
    }
    builtin_hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    builtin_hits.truncate(commands::MAX_RESULTS);

    // Open windows (§7.5). The enumeration is cached, so the per-keystroke
    // cost is a match over a few dozen titles.
    let mut window_hits = winman::match_query(
        &win_cache.snapshot(),
        &text,
        winman::MAX_RESULTS * FRECENCY_POOL,
    );
    for it in &mut window_hits {
        it.score += frec.bonus(&it.id);
    }
    window_hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    window_hits.truncate(winman::MAX_RESULTS);

    let calc = calc::evaluate(&text).map(|r| CalcRow {
        display: r.display,
        value: r.copy,
    });

    log::debug!(
        "shell gen={gen}: {} app(s), {} setting(s), {} command(s), {} window(s), calc={}",
        apps.len(),
        settings.len(),
        builtin_hits.len(),
        window_hits.len(),
        calc.is_some()
    );
    if let Err(e) = app.emit(
        "search:shell",
        ShellResults {
            gen,
            apps,
            settings,
            commands: builtin_hits,
            windows: window_hits,
            calc,
        },
    ) {
        log::warn!("emit search:shell failed: {e}");
    }
    // The service half is enqueued after, so a slow pipe cannot delay the
    // rows the shell already has. With no service — portable mode (§9.5), or
    // one that has not come up yet — the shell answers file search itself
    // through the same provider §3.1 routes unsupported scopes to.
    if !pipe.is_connected() {
        run_fallback_search(&app, gen, text, "fast indexing off");
        return Ok(Accepted { accepted: true });
    }
    pipe.search(gen, text).map_err(|e| {
        log::warn!("search cmd failed: {e}");
        e
    })?;
    Ok(Accepted { accepted: true })
}

/// §7.3's `kind:` categories, as extension sets. The service ignores `kind`
/// (M0 left it for the shell), so it is expanded here into the `ext` filter
/// the service does honour. `folder` is not an extension and needs a
/// directory flag the protocol does not carry yet, so it is refused by name.
const KIND_EXTS: &[(&str, &[&str])] = &[
    (
        "document",
        &[
            "pdf", "doc", "docx", "odt", "rtf", "txt", "md", "xls", "xlsx", "ods", "csv", "ppt",
            "pptx", "odp",
        ],
    ),
    (
        "image",
        &[
            "png", "jpg", "jpeg", "gif", "webp", "bmp", "svg", "heic", "tif", "tiff", "ico",
        ],
    ),
    (
        "audio",
        &["mp3", "flac", "wav", "m4a", "aac", "ogg", "opus", "wma"],
    ),
    ("video", &["mp4", "mkv", "mov", "avi", "webm", "m4v", "wmv"]),
    (
        "archive",
        &["zip", "7z", "rar", "tar", "gz", "bz2", "xz", "zst", "iso"],
    ),
];

/// Split a File Search query into the name query and §7.3's filters.
///
/// `kind:image`, `ext:rs,toml`, `path:src` — case-insensitive on the key,
/// repeatable (`ext:` accumulates, later `path:`/`kind:` win), combined with
/// AND by the service. Everything else is the name. Returns the reason when a
/// filter cannot be honoured, so the view can say so instead of applying it
/// to nothing.
fn parse_file_query(raw: &str) -> Result<(String, Filters), String> {
    let mut filters = Filters::default();
    let mut words: Vec<&str> = Vec::new();
    // Kept apart until the end: the service ORs everything in `ext`, and
    // §7.3 says the filters combine with AND, so `kind:image ext:png` has to
    // become the INTERSECTION — not the union that pouring both into one list
    // would give.
    let mut kind_exts: Option<Vec<String>> = None;
    let mut exts: Vec<String> = Vec::new();
    for token in raw.split_whitespace() {
        // A drive letter is not a filter key: `c:\\users` is a name to search
        // for, however much it looks like `x:value`.
        let Some((key, value)) = token.split_once(':').filter(|(k, _)| k.len() > 1) else {
            words.push(token);
            continue;
        };
        match key.to_ascii_lowercase().as_str() {
            "ext" => exts.extend(
                value
                    .split(',')
                    .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
                    .filter(|e| !e.is_empty()),
            ),
            "path" if !value.is_empty() => filters.path_substr = Some(value.to_string()),
            // A bare `path:` is a filter not yet typed, like a bare `kind:` —
            // not a name to search for. Without this arm it fell through to
            // the words and went to the index as the literal "path:".
            "path" => {}
            "kind" => {
                let want = value.to_ascii_lowercase();
                if want == "folder" {
                    return Err(
                        "kind:folder is not available yet — the index carries no folder filter"
                            .to_string(),
                    );
                }
                match KIND_EXTS.iter().find(|(k, _)| *k == want) {
                    Some((_, set)) => kind_exts = Some(set.iter().map(|e| e.to_string()).collect()),
                    // Typed so far, not typed wrong: `kind:im` is on its way to
                    // `kind:image`, and an error on every keystroke between
                    // would blank the list five times. A prefix of a known kind
                    // is no filter yet; the name still searches.
                    None if KIND_EXTS.iter().any(|(k, _)| k.starts_with(want.as_str())) => {}
                    None => {
                        return Err(format!(
                            "unknown kind:{value}; try document, image, audio, video or archive"
                        ))
                    }
                }
            }
            // `content:` is M2's full-text index (§3.5); until then it is a
            // word like any other, which is the least surprising fallback.
            _ => words.push(token),
        }
    }
    filters.ext = match (kind_exts, exts.is_empty()) {
        (Some(set), false) => {
            let both: Vec<String> = exts.iter().filter(|e| set.contains(e)).cloned().collect();
            if !both.is_empty() {
                both
            } else if exts
                .iter()
                .any(|e| set.iter().any(|k| k.starts_with(e.as_str())))
            {
                // `kind:image ext:p` on the way to `png`: not disjoint yet,
                // just unfinished. The kind alone applies until it is.
                set
            } else {
                return Err(
                    "that ext: is not one of the kind: you asked for, so nothing could match"
                        .to_string(),
                );
            }
        }
        (Some(set), true) => set,
        (None, _) => exts,
    };
    let name = words.join(" ");
    // The index answers a name query; a filter on its own would ask it to
    // list everything of a kind, which it cannot. Said, rather than answered
    // with a silent "no files match". That includes the not-yet-a-filter
    // forms — a bare `kind:` or `ext:`, a half-typed `kind:im` — which set
    // nothing and would otherwise send an empty query.
    if name.is_empty() && !raw.trim().is_empty() {
        return Err(
            "add a word from the name — filters narrow a search, they cannot list the index on their own"
                .to_string(),
        );
    }
    Ok((name, filters))
}

/// §7.3's File Search: the same index and the same generation counter as the
/// root list, a long page instead of a short one, and the query parsed for
/// filters first. Results arrive on the same events; the view keeps the ones
/// for its generation.
#[tauri::command]
fn files_search(
    app: AppHandle,
    pipe: tauri::State<'_, Arc<PipeClient>>,
    gen: u64,
    text: String,
) -> Result<Accepted, String> {
    // §7.3 has the service stat size/mtime for the returned page, and this
    // view shows neither — so the page is long enough to be the "full list"
    // the root list defers to, and no longer.
    const PAGE: u32 = 100;
    // An empty box, or a query the parser refuses, must not leave the last
    // long page running on the service for nothing: the pipe has no other
    // way to learn this view stopped caring.
    if text.trim().is_empty() {
        let _ = pipe.cancel_current();
        return Ok(Accepted { accepted: true });
    }
    let (name, filters) = match parse_file_query(&text) {
        Ok(parsed) => parsed,
        Err(e) => {
            let _ = pipe.cancel_current();
            return Err(e);
        }
    };
    log::debug!(
        "files cmd: gen={gen} len={} ext={} path={}",
        name.chars().count(),
        filters.ext.len(),
        filters.path_substr.is_some()
    );
    if !pipe.is_connected() {
        // Windows Search takes the name only; the view says the filters did
        // not apply.
        run_fallback_search(&app, gen, name, "fast indexing off");
        return Ok(Accepted { accepted: true });
    }
    pipe.search_with(gen, name, filters, PAGE)?;
    Ok(Accepted { accepted: true })
}

/// §7.1/§7.2 icon extraction, off the query path: the frontend asks per
/// visible row and renders its glyph until this resolves (§5.10).
///
/// Takes `(kind, id)` — the same pair `execute_action` takes — and never a
/// parsing name. The webview cannot name a shell item to activate; the shell
/// derives one from its own catalogs, or returns `None` and the row keeps its
/// glyph. See [`row_icons`] for why that boundary is where it is.
#[tauri::command]
async fn row_icon(
    cache: tauri::State<'_, Arc<IconCache>>,
    apps: tauri::State<'_, Arc<AppCatalog>>,
    settings: tauri::State<'_, Arc<SettingsCatalog>>,
    kind: String,
    id: String,
    px: i32,
) -> Result<Option<String>, String> {
    let cache = cache.inner().clone();
    let apps = apps.inner().clone();
    let settings = settings.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let Some(name) = row_icons::resolve(&kind, &id, &apps, &settings)? else {
            return Ok(None);
        };
        cache.icon(&name, px).map(|uri| Some(uri.to_string()))
    })
    .await
    .map_err(|e| format!("icon task: {e}"))?
}

#[tauri::command]
fn hide_window(app: AppHandle) -> Result<(), String> {
    dismiss(&app);
    Ok(())
}

/// §10 M0 harness marker relay: the frontend cannot write ETW itself, so its
/// measurement points (results applied in a rAF; hidden-rAF throttling
/// observations) arrive here and go out through [`etw_mark::mark`].
///
/// Whitelisted by prefix: this is an unauthenticated local IPC surface, and a
/// page bug must not be able to spray arbitrary strings into a trace someone
/// is reading measurements off.
#[tauri::command]
fn m0_mark(text: String) -> Result<(), String> {
    const ALLOWED: [&str; 2] = ["applied ", "rafgap "];
    if !ALLOWED.iter().any(|p| text.starts_with(p)) || text.len() > 128 {
        log::debug!("m0_mark rejected: {text:?}");
        return Err("m0_mark: unrecognized marker".to_string());
    }
    // Debug on purpose: this line is the only process-local proof that the
    // FRONTEND half of the instrumentation is alive — a webview serving stale
    // cached JS produces Rust-side markers and silence here, which is
    // indistinguishable from "working" in the ETW stream alone.
    log::debug!("m0_mark: {text}");
    etw_mark::mark(&text);
    Ok(())
}

#[tauri::command]
fn frontend_ready(
    app: AppHandle,
    pipe: tauri::State<'_, Arc<PipeClient>>,
    warm: tauri::State<'_, WarmState>,
) -> Result<(), String> {
    let was_warm = warm.0.swap(true, Ordering::SeqCst);
    if !was_warm {
        log::info!("frontend ready: first frame rendered, renderer warm (§5.4)");
    } else {
        log::debug!("frontend ready (reload)");
    }
    // The connect event may have fired before the frontend was listening.
    pipe_client::emit_conn_state(&app, pipe.is_connected());
    // And so may a view request: `--settings`, first-run onboarding, or
    // §5.1's conflict remedy all run during `setup`, before this page existed.
    // Taken, not cloned — a reload must not reopen a view the user has since
    // left.
    if let Some(pending) = app.try_state::<PendingView>() {
        let view = pending.0.lock().ok().and_then(|mut g| g.take());
        if let Some(view) = view {
            log::debug!("frontend ready: replaying {view}");
            if let Err(e) = app.emit(&view, ()) {
                log::warn!("could not replay {view}: {e}");
            }
        }
    }
    Ok(())
}

/// §10 M0 non-injecting self-measurement request, read by the frontend on
/// startup. `queries;iterations` (e.g. `kernel,ntdll,win;30`) drives the real
/// keydown→results path IN THE PAGE — no SendInput, no ETW session, no window
/// focus. Empty when not requested.
#[tauri::command]
fn m0_spec() -> String {
    std::env::var("YSPOT_M0_SELFMEASURE").unwrap_or_default()
}

/// §10 M0: receive the self-measurement samples from the frontend and log the
/// per-bucket keydown→results p50/p95 against the ≤ 20 ms §2.5 budget.
#[tauri::command]
fn m0_report(json: String) -> Result<(), String> {
    #[derive(serde::Deserialize)]
    struct Sample {
        q: String,
        plen: usize,
        ms: f64,
    }
    #[derive(serde::Deserialize)]
    struct Report {
        #[serde(default)]
        iterations: u32,
        #[serde(default)]
        rows: Vec<Sample>,
        #[serde(default)]
        error: Option<String>,
    }
    log::info!("m0 selfmeasure raw report: {json}");
    let rep: Report = serde_json::from_str(&json).map_err(|e| e.to_string())?;
    if let Some(err) = rep.error {
        log::error!("m0 selfmeasure: frontend reported error: {err}");
        return Ok(());
    }
    if rep.rows.is_empty() {
        return Ok(());
    }
    log::info!(
        "m0 selfmeasure results — {} iterations, {} samples, keydown→results (frontend clock, \
         §2.5 gate p95 ≤ 20 ms):",
        rep.iterations,
        rep.rows.len()
    );
    // Bucket by (query, prefix length), same shape as `yspot-m0 type`.
    let mut buckets: std::collections::BTreeMap<(String, usize), Vec<f64>> = Default::default();
    for s in rep.rows {
        buckets.entry((s.q, s.plen)).or_default().push(s.ms);
    }
    for ((q, plen), mut v) in buckets {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let at = |frac: f64| v[((frac * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1];
        let (p50, p95, max) = (at(0.50), at(0.95), v[v.len() - 1]);
        let prefix: String = q.chars().take(plen).collect();
        log::info!(
            "  {prefix:<12} ({plen}c) n={:<3} p50 {p50:>6.2}  p95 {p95:>6.2}  max {max:>6.2} ms  {}",
            v.len(),
            if p95 <= 20.0 { "PASS" } else { "FAIL" }
        );
    }
    Ok(())
}

/// §4.6 `executeAction`: run an action on a result.
///
/// `kind` selects the provider (`"app"` or `"file"`), `id` is that
/// provider's stable id (§5.6: AUMID for apps, `volumeIdx:frn` for files),
/// and `action` names the verb (§7.1, §7.3). Files carry their path because
/// the shell holds no file catalog of its own — the service's row is the
/// record.
///
/// Every file action runs here, in the unelevated shell, never in the
/// elevated service (§7.3).
#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn execute_action(
    app: AppHandle,
    frec: tauri::State<'_, Arc<Frecency>>,
    catalog: tauri::State<'_, Arc<AppCatalog>>,
    settings: tauri::State<'_, Arc<SettingsCatalog>>,
    kind: String,
    id: String,
    path: Option<String>,
    action: Option<String>,
) -> Result<(), String> {
    let action = action.unwrap_or_else(|| "open".to_string());
    // Clipboard actions leave the launcher up: copying a path is usually a
    // step, not the end of the errand. Everything that hands the user to
    // another window dismisses first, so the launcher is not left floating
    // over what it just opened.
    // Actions that keep the launcher up: copying is usually a step rather
    // than the end of the errand, and Settings opens IN the launcher, so
    // dismissing would close the thing the user just asked for.
    let stays_open = matches!(action.as_str(), "copy_path" | "copy_file" | "copy")
        || (kind == "command"
            && matches!(
                id.as_str(),
                "yspot.settings" | "yspot.clipboard" | "yspot.files"
            ));
    if !stays_open {
        // Dismissed BEFORE the action so focus lands on whatever the action
        // raises (§5.2 hands focus back to the previous foreground window,
        // and a dialog opened after that would otherwise fight it).
        dismiss(&app);
    }
    let result = match kind.as_str() {
        "app" => {
            let entries = catalog.snapshot();
            let entry = entries
                .iter()
                .find(|e| e.aumid == id)
                .ok_or_else(|| format!("unknown app {id}"));
            entry.and_then(|entry| {
                apps::launch(&entry.aumid, entry.kind, action == "runas").map(|()| true)
            })
        }
        "command" => match id.as_str() {
            "yspot.settings" => show_settings(&app).map(|()| false),
            // §7.2: the Settings home, opened exactly as the catalog opened it
            // before it moved into this list. Deliberately absent from
            // `stays_open` above — it hands the user to another application,
            // so the launcher gets out of the way first.
            "windows.settings" => shell_open("ms-settings:").map(|()| true),
            // §7.6: the Windows Backup app, opened exactly as its AppsFolder
            // row would have opened it — that row is suppressed as this
            // command's duplicate. Hands the user to another application, so
            // like the Settings home it is absent from `stays_open`.
            "windows.backup" => {
                apps::launch(apps::WINDOWS_BACKUP_AUMID, apps::AppKind::Packaged, false)
                    .map(|()| true)
            }
            "yspot.clipboard" => show_view(&app, "view:clipboard").map(|()| false),
            "yspot.files" => show_view(&app, "view:files").map(|()| false),
            "yspot.quit" => {
                log::info!("quit from the launcher; the indexing service keeps running (§5.5)");
                app.exit(0);
                Ok(false)
            }
            other => Err(format!("unknown command {other}")),
        },
        // §7.5: switch, lay out, pin, minimize or close a window. Switching
        // is what counts as use; a layout change is not a launch.
        "window" => winman::act(&id, &action).map(|()| action == "open" || action == "switch"),
        "setting" => {
            let entry = settings
                .find(&id)
                .ok_or_else(|| format!("unknown settings entry {id}"))?;
            settings_catalog::launch(entry).map(|()| true)
        }
        // §7.7: Enter copies the calculator's answer. The value travels in
        // `path` because that is the field already carrying a row's payload;
        // there is nothing on disk to open.
        "calc" => {
            let value = path.ok_or_else(|| "calc action needs a value".to_string())?;
            file_actions::copy_text(&value).map(|()| false)
        }
        "file" => match path {
            None => Err("file action needs a path".to_string()),
            Some(path) => match action.as_str() {
                "open" => shell_open(&path).map(|()| true),
                "open_with" => file_actions::open_with(&path).map(|()| false),
                "reveal" => file_actions::reveal(&path).map(|()| false),
                "copy_path" => file_actions::copy_path(&path).map(|()| false),
                "copy_file" => file_actions::copy_file(&path).map(|()| false),
                "delete" => file_actions::delete_to_recycle_bin(&path).map(|()| false),
                other => Err(format!("unknown file action {other}")),
            },
        },
        other => Err(format!("unknown result kind {other}")),
    };
    match result {
        // Only a launch counts as use (§7.1 frecency is launch count); copying
        // a path or revealing a folder is not what the ranking is about.
        Ok(counts_as_launch) => {
            if counts_as_launch {
                frec.record(&id, &kind);
            }
            Ok(())
        }
        Err(e) => {
            log::warn!("action {action} on {kind} {id} failed: {e}");
            Err(e)
        }
    }
}

/// Results from the shell's own Windows Search provider (§3.1, §9.5), which
/// answers when there is no service (portable mode) or when the service says
/// a scope is not one it indexes.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct FallbackResults {
    gen: u64,
    items: Vec<FallbackItem>,
    /// Why the shell answered instead of the service — shown as §9.5's
    /// unobtrusive hint, never a nag.
    reason: String,
    /// Set when Windows Search itself could not answer: §3.1 requires this
    /// to read as "not searchable", never as an empty result.
    unavailable: Option<String>,
}

/// How many candidates each catalog source is asked for, as a multiple of what
/// it will actually show.
///
/// §7.1 says app results are frecency-ranked. They were not: every source cut
/// itself to its display size and only THEN got the bonus, so frecency could
/// reorder the page but never change who was on it. The matcher's tiers are
/// flat constants — every prefix hit scores exactly 0.9 — so a one-letter
/// query is one big tie broken by name length, and "Sublime Text" lost to
/// "Steam" every time however often it was launched. Widening the pool first
/// costs a longer truncate on a list already fully sorted.
const FRECENCY_POOL: usize = 8;

/// Where an unranked Windows Search hit sits against the §5.11 bands. Below
/// the catalog's own matches (`matcher::SUBSTRING` and up), because the shell
/// scored those and merely knows these exist.
const FALLBACK_BASE_SCORE: f32 = 0.5;

/// Windows Search hands us no score of its own, so the base has to sit below
/// the catalog's own bands (§5.11) or portable mode would bury real matches
/// under whatever the OS index happened to return. Checked at compile time
/// because both sides are constants and neither should drift into the other.
///
/// This bounds the *base*, deliberately, not the base plus frecency. A file
/// you open daily is meant to climb past a substring match you have never
/// opened — that is what §7.1 is for, and apps, settings and windows have
/// always behaved that way. The bound that matters is that an *unranked* hit
/// does not start above a scored one.
const _: () = assert!(FALLBACK_BASE_SCORE < matcher::SUBSTRING);

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct FallbackItem {
    path: String,
    name: String,
    /// Windows Search does not rank in our terms, so these rows sit at one
    /// flat score below the catalog bands — plus §7.1 frecency, which is the
    /// only signal the shell has about them and the reason a file you open
    /// daily comes back first (§5.11 rule 2).
    score: f32,
}

struct FallbackRequest {
    gen: u64,
    text: String,
    reason: String,
}

/// One shell-side file search at a time, and only the newest one.
///
/// The provider is a blocking OLE DB round trip into Windows Search that
/// cannot be cancelled once it has started. Spawning one per keystroke meant
/// "readme.md" put nine of them on the blocking pool at once, every result but
/// the last thrown away — while the query the user is actually waiting on
/// queued behind eight dead ones.
///
/// So: a single worker with a single-slot mailbox, which is the shape §4.4
/// already defines for the service path. A request arriving while another is
/// still waiting replaces it, and a result whose generation has been
/// superseded is dropped rather than emitted.
#[derive(Default)]
struct FallbackQueue {
    /// The newest request that has not started yet. One slot, deliberately.
    slot: Mutex<Option<FallbackRequest>>,
    ready: std::sync::Condvar,
    /// The newest generation anyone has asked for.
    latest: AtomicU64,
}

impl FallbackQueue {
    fn submit(&self, req: FallbackRequest) {
        self.latest.fetch_max(req.gen, Ordering::SeqCst);
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some(req);
        self.ready.notify_one();
    }

    /// Block until there is something to do.
    fn take(&self) -> FallbackRequest {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(req) = slot.take() {
                return req;
            }
            slot = self.ready.wait(slot).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Whether a result for `gen` is still worth emitting.
    fn is_current(&self, gen: u64) -> bool {
        self.latest.load(Ordering::SeqCst) <= gen
    }
}

/// Ask for a shell-side file search. Returns immediately.
///
/// The single entry point §9.5 asks for: portable mode and §3.1's `107`
/// routing are the same code path, not two implementations.
pub(crate) fn run_fallback_search(app: &AppHandle, gen: u64, text: String, reason: &str) {
    let Some(queue) = app.try_state::<Arc<FallbackQueue>>() else {
        log::warn!("fallback search asked for before the worker existed; dropped");
        return;
    };
    queue.submit(FallbackRequest {
        gen,
        text,
        reason: reason.to_string(),
    });
}

/// The worker: one blocking query at a time, newest first.
fn fallback_worker(app: AppHandle, queue: Arc<FallbackQueue>) {
    loop {
        let req = queue.take();
        // Checked before the query as well as after: a request can be
        // superseded while it sits in the slot, and the cheapest OLE DB round
        // trip is the one never made.
        if !queue.is_current(req.gen) {
            log::debug!("fallback gen={}: superseded before it ran", req.gen);
            continue;
        }
        run_fallback_query(&app, req, &queue);
    }
}

fn run_fallback_query(app: &AppHandle, req: FallbackRequest, queue: &FallbackQueue) {
    let FallbackRequest { gen, text, reason } = req;
    {
        let Some(provider) = app.try_state::<Arc<dyn FileSearch>>() else {
            return;
        };
        // Absent only in tests that never register it; a missing store must
        // cost the rows their bonus, not their existence.
        let frec = app.try_state::<Arc<Frecency>>().map(|f| f.inner().clone());
        let payload = match provider.search(&text, 50) {
            Ok(hits) => {
                let mut items: Vec<FallbackItem> = hits
                    .into_iter()
                    .map(|h| FallbackItem {
                        // The path is this provider's stable id — it has no
                        // volume/FRN identity — and so is what a launch from
                        // one of these rows records against (§5.6).
                        score: FALLBACK_BASE_SCORE
                            + frec.as_ref().map_or(0.0, |f| f.bonus(&h.path)),
                        path: h.path,
                        name: h.name,
                    })
                    .collect();
                items.sort_by(|a, b| b.score.total_cmp(&a.score));
                FallbackResults {
                    gen,
                    items,
                    reason,
                    unavailable: None,
                }
            }
            Err(search_fallback::SearchError::Unavailable(m)) => FallbackResults {
                gen,
                items: Vec::new(),
                reason,
                unavailable: Some(m),
            },
            Err(e) => {
                log::warn!("fallback search failed: {e}");
                FallbackResults {
                    gen,
                    items: Vec::new(),
                    reason,
                    unavailable: Some(e.to_string()),
                }
            }
        };
        log::debug!(
            "fallback gen={gen}: {} hit(s){}",
            payload.items.len(),
            payload
                .unavailable
                .as_ref()
                .map(|m| format!(" (unavailable: {m})"))
                .unwrap_or_default()
        );
        // A newer keystroke landed while this was in the provider. The rows
        // are for a query the user has already moved past, so they are
        // dropped rather than raced against the result that is coming.
        if !queue.is_current(gen) {
            log::debug!(
                "fallback gen={gen}: superseded while running; {} hit(s) dropped",
                payload.items.len()
            );
            return;
        }
        if let Err(e) = app.emit("search:fallback", payload) {
            log::warn!("emit search:fallback failed: {e}");
        }
    }
}

/// §7.4 clipboard history, for the view that shows it. An empty query lists
/// the most recent entries, which is what opening the view should do.
#[tauri::command]
fn clipboard_list(
    clip: tauri::State<'_, Arc<ClipboardStore>>,
    query: String,
) -> Vec<clipboard::ClipMatch> {
    clip.match_query(&query, clipboard::MAX_RESULTS)
}

/// §7.4 paste: hide the launcher, hand focus back to where it came from,
/// write the entry to the clipboard and inject Ctrl+V.
#[tauri::command]
fn clipboard_paste(
    app: AppHandle,
    clip: tauri::State<'_, Arc<ClipboardStore>>,
    id: i64,
) -> Result<(), String> {
    let text = clip
        .content(id)
        .ok_or_else(|| format!("clipboard entry {id} is gone"))?;
    // Dismiss FIRST: the paste needs the previous foreground window back, and
    // that cannot happen while the launcher still holds focus. What it hands
    // back is the window Ctrl+V is allowed to go to, and nothing else.
    let restored = dismiss(&app);
    clipboard::paste(&text, restored)
}

#[tauri::command]
fn clipboard_delete(clip: tauri::State<'_, Arc<ClipboardStore>>, id: i64) -> Result<(), String> {
    clip.delete(id)
}

#[tauri::command]
fn clipboard_clear(clip: tauri::State<'_, Arc<ClipboardStore>>) -> Result<(), String> {
    clip.clear()
}

#[tauri::command]
fn clipboard_enabled(clip: tauri::State<'_, Arc<ClipboardStore>>) -> bool {
    clip.is_enabled()
}

/// §7.4: pause or resume capture, and remember which. The store holds the
/// live flag; `settings.json` holds the decision, so it survives the restart
/// the user is most likely to make right after pausing.
#[tauri::command]
fn clipboard_set_enabled(
    clip: tauri::State<'_, Arc<ClipboardStore>>,
    store: tauri::State<'_, Arc<SettingsStore>>,
    enabled: bool,
) -> Result<(), String> {
    clip.set_enabled(enabled);
    let mut next = store.get();
    next.clipboard.capture = enabled;
    store.save(next)
}

/// What to say about a chord: what the registrar actually reported, and only
/// failing that, what the static check says.
///
/// The order is the whole point. A chord that came out of settings has
/// already passed [`settings::Hotkey::rejection`] — it was checked before it
/// was written — so consulting only the static check reports every real
/// conflict as "no problem", which is §5.1's silent degradation exactly.
fn hotkey_error(live: Option<String>, hotkey: &settings::Hotkey) -> Option<String> {
    live.or_else(|| hotkey.rejection().map(str::to_string))
}

/// What first-run onboarding needs to tell the truth about this machine
/// (§5.9): whether the hotkey took, whether the service is there, and what
/// the current consents are.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OnboardingState {
    hotkey: settings::Hotkey,
    /// Set when the chord could not be registered — §5.1's conflict flow.
    hotkey_error: Option<String>,
    /// Whether the index service is answering. §5.9's step 2 is consent to
    /// install it; until the service MSI exists there is nothing to launch,
    /// so the wizard reports the state and offers the skip path.
    service_connected: bool,
    autostart: bool,
    crash_reports: bool,
}

#[tauri::command]
fn onboarding_state(
    app: AppHandle,
    store: tauri::State<'_, Arc<SettingsStore>>,
    pipe: tauri::State<'_, Arc<PipeClient>>,
) -> OnboardingState {
    let settings = store.get();
    OnboardingState {
        hotkey_error: hotkey_error(HotkeyState::get(&app), &settings.hotkey),
        hotkey: settings.hotkey,
        service_connected: pipe.is_connected(),
        autostart: autostart::is_enabled(),
        crash_reports: settings.diagnostics.crash_reports,
    }
}

/// §5.9: finishing the wizard records that it happened, so it is shown once.
#[tauri::command]
fn finish_onboarding(
    app: AppHandle,
    store: tauri::State<'_, Arc<SettingsStore>>,
) -> Result<(), String> {
    let mut next = store.get();
    next.onboarded = true;
    store.save(next)?;
    log::info!("onboarding complete (§5.9)");
    let _ = app.emit("view:reset", ());
    Ok(())
}

/// §5.9 Settings: the current settings plus what the UI needs to render
/// them honestly (whether autostart is really on, what the hotkey warns
/// about).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsView {
    settings: Settings,
    autostart: bool,
    hotkey_warning: Option<String>,
    /// §5.1's conflict, standing: set whenever the bound chord is not
    /// actually registered, so opening Settings shows the alternatives
    /// without the user having to fail a rebind first.
    hotkey_error: Option<String>,
}

#[tauri::command]
fn get_settings(app: AppHandle, store: tauri::State<'_, Arc<SettingsStore>>) -> SettingsView {
    let settings = store.get();
    SettingsView {
        hotkey_warning: settings.hotkey.warning().map(str::to_string),
        hotkey_error: HotkeyState::get(&app),
        autostart: autostart::is_enabled(),
        settings,
    }
}

/// What a settings save asks of the registrar.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Registrar {
    /// Nothing about the chord or its owner changed, and the chord is held.
    Nothing,
    /// The user handed the chord to YKeys: release ours if we hold it.
    HandToYkeys,
    /// The user took the chord back from YKeys: bind it, from nothing.
    TakeBack,
    /// A different chord, shell-owned: §5.1's atomic replace.
    Replace,
    /// The same chord, shell-owned, but not held: try again.
    Retry,
}

/// The decision table for [`save_settings`], on its own so it can be read
/// and tested as one.
///
/// Two things it has to get right that a simple "did the chord change" did
/// not. `hotkey_source` is a runtime setting: flipping it has to (un)register
/// now, not at the next restart, or the toast lies. And a contested chord is
/// not a reason to refuse an unrelated save — the theme, a consent, or the
/// YKeys hand-off that is the very remedy for the conflict — which is what
/// happened when every save while contested went through the registrar and
/// took its `Err` as the save's.
fn registrar_action(current: &Settings, next: &Settings, contested: bool) -> Registrar {
    use settings::HotkeySource::{Shell, Ykeys};
    match (current.hotkey_source, next.hotkey_source) {
        (_, Ykeys) => Registrar::HandToYkeys,
        (Ykeys, Shell) => Registrar::TakeBack,
        (Shell, Shell) if current.hotkey != next.hotkey => Registrar::Replace,
        (Shell, Shell) if contested => Registrar::Retry,
        (Shell, Shell) => Registrar::Nothing,
    }
}

/// §5.9: every mutation goes through the shell. A changed hotkey is rebound
/// atomically before anything is persisted, so the file can never name a
/// chord the running shell does not hold.
#[tauri::command]
fn save_settings(
    app: AppHandle,
    store: tauri::State<'_, Arc<SettingsStore>>,
    next: Settings,
) -> Result<SettingsView, String> {
    let current = store.get();
    let contested = HotkeyState::get(&app).is_some();
    match registrar_action(&current, &next, contested) {
        Registrar::Nothing => {}
        Registrar::HandToYkeys => {
            // Release ours if we hold one; a chord this process does not
            // register cannot be in conflict, so the standing one clears.
            if current.hotkey_source.registers_in_shell() && !contested {
                unregister_hotkey(&app, &current.hotkey);
            }
            set_standing_conflict(&app, None);
        }
        Registrar::TakeBack | Registrar::Retry => {
            // Nothing of ours is bound in either case, so there is nothing to
            // restore — and a refusal is a standing conflict on the chord the
            // settings hold, not a reason to refuse the save. §5.1's
            // alternatives include chords a user may already have, and
            // whoever took the chord may have let it go since startup, which
            // is why the unchanged case is worth a retry at all.
            let r = rebind_hotkey(&app, None, &next.hotkey);
            set_standing_conflict(&app, r.outcome.err());
        }
        Registrar::Replace => {
            // §5.1's atomic rule: the new chord binds or the old one stays,
            // and the file never names a chord this process does not hold.
            // The old chord is only ours to give back if we actually hold it.
            let old = (!contested).then_some(&current.hotkey);
            let r = rebind_hotkey(&app, old, &next.hotkey);
            if let Err(e) = r.outcome {
                // Refused, so the settings keep the OLD chord.
                //
                // Three cases. Old chord bound again: no conflict. Old chord
                // was never ours this session (`old` is None because it was
                // already contested): the standing message from startup —
                // with its likely-owner hint — is still the true one, and
                // must be left alone; `bound` is false here trivially, not
                // because anything was lost. Old chord GIVEN UP and not
                // recovered, because something claimed it in the
                // unregister→register window: that is §5.1's silent
                // degradation and the one case that needs a new message,
                // named for the chord the settings still hold.
                if r.bound {
                    set_standing_conflict(&app, None);
                }
                set_standing_conflict_if(
                    &app,
                    (old.is_some() && !r.bound).then(|| {
                        format!(
                            "{} is no longer registered, and {} could not take its place.",
                            current.hotkey.accelerator(),
                            next.hotkey.accelerator()
                        )
                    }),
                );
                return Err(e);
            }
            set_standing_conflict(&app, None);
        }
    }
    let warning = next.hotkey.warning().map(str::to_string);
    if next.diagnostics != current.diagnostics {
        // §8.5: consent takes effect now, not at the next start.
        if let Err(e) = diagnostics::set_crash_capture(next.diagnostics.crash_reports) {
            log::warn!("crash capture could not be configured: {e}");
        }
        diagnostics::rotate_dumps();
    }
    store.save(next.clone())?;
    // The launcher window is listening: a theme change applies at once
    // rather than at the next restart.
    if let Err(e) = app.emit("settings:changed", &next) {
        log::warn!("emit settings:changed failed: {e}");
    }
    Ok(SettingsView {
        settings: next,
        autostart: autostart::is_enabled(),
        hotkey_warning: warning,
        // A successful rebind cleared it; an unchanged chord that never
        // registered keeps it, so the banner does not vanish because an
        // unrelated setting was saved.
        hotkey_error: HotkeyState::get(&app),
    })
}

/// Settings opens INSIDE the launcher, as a view the search window grows to
/// fit, rather than as a separate framed window.
///
/// SPEC §5.9 called for a separate window; this is a deliberate amendment
/// (recorded there and in `docs/M1.md`). A launcher you already have open,
/// with your hands on the keys, should not throw a second window at the
/// taskbar to change a hotkey — you search for the thing, you get it in
/// place, and Esc takes you back to the query. §5.7 already models exactly
/// that as the navigation stack Esc pops.
#[tauri::command]
fn open_settings(app: AppHandle) -> Result<(), String> {
    show_settings(&app)
}

fn show_settings(app: &AppHandle) -> Result<(), String> {
    show_view(app, "view:settings")
}

/// Open one of the launcher's in-place views (§5.9 as amended, §5.7's
/// navigation stack). The launcher is summoned first if it is hidden — a
/// request from the tray arrives with no window on screen.
fn show_view(app: &AppHandle, event: &str) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("launcher") {
        if !window.is_visible().unwrap_or(false) {
            show(app);
        }
    }
    // Until the page has signalled ready once, this emit goes nowhere.
    let warm = app
        .try_state::<WarmState>()
        .is_some_and(|w| w.0.load(Ordering::SeqCst));
    if !warm {
        if let Some(pending) = app.try_state::<PendingView>() {
            if let Ok(mut g) = pending.0.lock() {
                *g = Some(event.to_string());
            }
        }
    }
    app.emit(event, ())
        .map_err(|e| format!("emit {event}: {e}"))?;
    log::info!("{event} opened in the launcher");
    Ok(())
}

/// Resize the launcher in place to `logical_height` logical pixels, keeping
/// its top edge and its horizontal position (§5.3's placement, recomputed
/// for the new height). The frontend calls this when it switches between the
/// results list and a taller view.
/// The frontend telling the shell which surface it is showing, so blur can
/// mean "dismiss" for the results list and nothing for a view with controls.
#[tauri::command]
fn set_in_view(view: tauri::State<'_, ViewState>, in_view: bool) {
    view.0.store(in_view, Ordering::SeqCst);
}

#[tauri::command]
fn set_launcher_height(app: AppHandle, logical_height: u32) -> Result<(), String> {
    let Some(window) = app.get_webview_window("launcher") else {
        return Ok(());
    };
    let logical = logical_height.clamp(120, 2000) as i32;
    // The monitor the launcher is ON, not the one the pointer wandered to:
    // this is a resize of a window already on screen, and §5.3's cursor rule
    // is for summoning. Falls back to the cursor only if the handle is gone.
    let hwnd = window.hwnd().map(|h| h.0 as isize).unwrap_or(0);
    let Some(p) = placement::compute_placement_for_window(hwnd, logical)
        .or_else(|| placement::compute_placement_of_height(logical))
    else {
        return Err("placement computation failed".to_string());
    };
    window
        .set_position(tauri::PhysicalPosition::new(p.x, p.y))
        .map_err(|e| format!("set_position: {e}"))?;
    window
        .set_size(tauri::PhysicalSize::new(p.width, p.height))
        .map_err(|e| format!("set_size: {e}"))
}

/// §5.4 autostart state, for the Settings UI (§5.9) and the tray toggle.
/// §5.9's "log export", as the honest version of it: open the folder the
/// logs and dumps are already in. A zip would need an archiver dependency
/// for something Explorer does better, and the §8.5 directories are plain
/// files the user can already read.
#[tauri::command]
fn open_diagnostics_folder() -> Result<(), String> {
    let dir = diagnostics::data_dir().ok_or_else(|| "LOCALAPPDATA unset".to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    shell_open(&dir.to_string_lossy())
}

#[tauri::command]
fn get_autostart() -> bool {
    autostart::is_enabled()
}

#[tauri::command]
fn set_autostart(enabled: bool) -> Result<(), String> {
    if enabled {
        autostart::enable()
    } else {
        autostart::disable()
    }
}

#[tauri::command]
fn get_status(pipe: tauri::State<'_, Arc<PipeClient>>) -> Result<(), String> {
    // Reply is relayed asynchronously as an `index:status` event.
    pipe.request_status()
}

/// `ShellExecuteW` with a null verb (default "open") and SW_SHOWNORMAL.
pub(crate) fn shell_open(path: &str) -> Result<(), String> {
    shell_execute_inner(path, None)
}

/// The same, with parameters — how §7.2 opens a Control Panel item
/// (`control.exe /name Microsoft.<CanonicalName>`).
pub(crate) fn shell_execute(file: &str, params: &str) -> Result<(), String> {
    shell_execute_inner(file, Some(params))
}

fn shell_execute_inner(path: &str, params: Option<&str>) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    if path.is_empty() {
        return Err("empty path".to_string());
    }
    let path_w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let params_w: Option<Vec<u16>> =
        params.map(|p| p.encode_utf16().chain(std::iter::once(0)).collect());
    // SAFETY: `path_w` and `params_w` are valid NUL-terminated UTF-16 strings
    // that outlive the call; null hwnd, verb and directory are documented as
    // permitted, as is a null parameter string.
    let inst = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            std::ptr::null(),
            path_w.as_ptr(),
            params_w.as_ref().map_or(std::ptr::null(), |p| p.as_ptr()),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // Values > 32 indicate success per the ShellExecuteW contract.
    let code = inst as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(format!("ShellExecuteW failed (code {code}) for {path}"))
    }
}

// ---------------------------------------------------------------------------

pub fn run() {
    // §8.5: a GUI process has no console, so the log has to go to a file or
    // it goes nowhere — which is every run that is not launched from a
    // terminal, i.e. every real one.
    diagnostics::init();
    // After the logger, because the hook logs through it: a panic across an
    // `extern "system"` boundary fast-fails past the exception filter, so
    // this is the only thing that records it (§8.5).
    diagnostics::install_panic_hook();
    etw_mark::init();

    if autostart::started_hidden() {
        // §5.4 warm start: the window is created hidden either way (the
        // Tauri config marks it invisible), so the flag only records why.
        log::info!("started with --hidden (autostart): warming the renderer, staying invisible");
    }

    let store = Arc::new(SettingsStore::open());
    let clip = ClipboardStore::open(store.get().clipboard.capture);
    let fallback_queue = Arc::new(FallbackQueue::default());
    // Behind the §11 Risk 6 trait, so the day Windows Search is not the
    // answer any more, only this line changes.
    let fallback: Arc<dyn FileSearch> = Arc::new(WindowsSearch);
    let pipe = Arc::new(PipeClient::new());
    let catalog = AppCatalog::new();
    let frec = Frecency::open();
    let icon_cache = IconCache::new();

    tauri::Builder::default()
        // §5.5 single instance: a second launch forwards a show to the
        // running shell and exits, so running the exe again summons the
        // launcher instead of starting a rival that fights for the hotkey.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            // §5.5: a second launch summons the running shell rather than
            // starting a rival. `--settings` is the one argument that means
            // something else — it is how a shortcut or a script opens the
            // Settings window without going through the tray.
            if argv.iter().any(|a| a == "--settings") {
                log::info!("second instance asked for Settings (§5.9)");
                if let Err(e) = show_settings(app) {
                    log::error!("could not open Settings: {e}");
                }
            } else {
                log::info!("second instance launched; summoning this one (§5.5)");
                show(app);
            }
        }))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    // Nothing on this path performs IPC or disk I/O (§5.1).
                    // Only one chord is ever registered, so a press is ours.
                    if matches!(event.state(), ShortcutState::Pressed) {
                        toggle(app);
                    }
                })
                .build(),
        )
        .manage(store)
        .manage(clip.clone())
        .manage(fallback)
        .manage(ViewState::default())
        .manage(Arc::new(commands::all()))
        .manage(winman::WindowCache::new())
        .manage(pipe.clone())
        .manage(catalog.clone())
        .manage(frec)
        .manage(icon_cache)
        .manage(WarmState::default())
        .manage(HotkeyState::default())
        .manage(PendingView::default())
        .manage(fallback_queue.clone())
        .invoke_handler(tauri::generate_handler![
            search,
            hide_window,
            frontend_ready,
            execute_action,
            row_icon,
            get_settings,
            save_settings,
            open_settings,
            onboarding_state,
            finish_onboarding,
            clipboard_list,
            files_search,
            clipboard_paste,
            clipboard_delete,
            clipboard_clear,
            clipboard_enabled,
            clipboard_set_enabled,
            set_launcher_height,
            set_in_view,
            get_autostart,
            set_autostart,
            open_diagnostics_folder,
            get_status,
            m0_mark,
            m0_report,
            m0_spec
        ])
        .setup(move |app| {
            pipe_client::spawn(app.handle().clone(), pipe.clone());

            // §7.2: the catalog file ships beside the executable so it can be
            // updated without a code release; the compiled-in copy is the
            // floor when it is missing (a dev build, a test, a broken
            // install). Gates are evaluated once, here.
            let resource_dir = app.path().resource_dir().ok();
            app.manage(SettingsCatalog::load(
                resource_dir.as_deref(),
                settings_catalog::probe_capabilities(),
            ));
            // §7.1: first enumeration now, then every 30 minutes; a show
            // with a stale catalog triggers one too. All off-thread.
            catalog.refresh_async();
            catalog.spawn_periodic();

            // §5.5: the tray icon is the shell's only always-available
            // surface, so a failure to create it is worth an error rather
            // than a silent absence — but not worth refusing to run.
            if let Err(e) = tray::build(app.handle()) {
                log::error!("tray icon could not be created: {e}");
            }

            // §7.4: the capture listener owns a message-only window and its
            // own message loop, so it runs on a thread of its own.
            clipboard::spawn_listener(clip.clone());

            // §8.5: crash capture follows the stored consent on every start,
            // so revoking it on one machine is not undone by a restart, and
            // dumps left behind are rotated away either way.
            let consent = app.state::<Arc<SettingsStore>>().get().diagnostics;
            if let Err(e) = diagnostics::set_crash_capture(consent.crash_reports) {
                log::warn!("crash capture could not be configured: {e}");
            }
            diagnostics::rotate_dumps();

            // `yspot.exe --settings` opens Settings directly, for a shortcut
            // or a script; the launcher itself stays hidden.
            if std::env::args().any(|a| a == "--settings") {
                if let Err(e) = show_settings(app.handle()) {
                    log::error!("could not open Settings: {e}");
                }
            }

            // §5.1: the binding persists in settings and re-registers on
            // every start. Before onboarding, so the wizard's first read of
            // `hotkey_error` already knows the answer.
            // §5.1 as amended: the shell listens for an external summons however
            // the chord is owned, so binding a second verb through YKeys does
            // not require flipping a setting first (see `hotkey_signal`).
            hotkey_signal::spawn(app.handle().clone());

            // §9.5's file search, on one thread of its own: the provider blocks
            // and cannot be cancelled, so serialising is what keeps a burst of
            // keystrokes from queueing behind each other's dead queries.
            {
                let handle = app.handle().clone();
                let queue = fallback_queue.clone();
                if let Err(e) = std::thread::Builder::new()
                    .name("file-fallback".into())
                    .spawn(move || fallback_worker(handle, queue))
                {
                    log::error!("fallback search worker failed to spawn: {e}");
                }
            }

            let current = app.state::<Arc<SettingsStore>>().get();
            let hotkey_failed = if current.hotkey_source.registers_in_shell() {
                let r = rebind_hotkey(app.handle(), None, &current.hotkey);
                set_standing_conflict(app.handle(), r.outcome.as_ref().err().cloned());
                match r.outcome {
                    Ok(()) => false,
                    Err(e) => {
                        log::error!("hotkey registration failed: {e} Rebind it in Settings.");
                        true
                    }
                }
            } else {
                // Not a degradation and not reported as one: the user asked for
                // this. §5.1's duty is to be honest about a chord that should
                // work and does not, and here no chord was ever ours to lose.
                log::info!(
                    "hotkey: YKeys owns the chord; summoned by message on class {} (§5.1)",
                    hotkey_signal::WINDOW_CLASS
                );
                false
            };
            let onboarded = current.onboarded;

            // §5.9 first run. Not when started hidden by autostart: a window
            // appearing unbidden at logon is exactly what --hidden promises
            // it will not do, so the wizard waits for the first summon.
            if !onboarded && !autostart::started_hidden() {
                if let Err(e) = show_view(app.handle(), "view:onboarding") {
                    log::error!("could not open onboarding: {e}");
                }
            } else if hotkey_failed && !autostart::started_hidden() {
                // §5.1 MUST NOT degrade silently, and a launcher with no
                // working hotkey and no window is as silent as it gets. This
                // is the one start where saying so costs nothing: the user
                // launched YSpot by hand a moment ago and is watching. A
                // hidden autostart gets the tray tooltip and the log instead
                // — a window at logon is the thing --hidden promises not to
                // do, and §5.1's remedy keeps until the first summon.
                log::info!("hotkey unavailable at startup; opening Settings (§5.1)");
                if let Err(e) = show_settings(app.handle()) {
                    log::error!("could not open Settings for the hotkey conflict: {e}");
                }
            }

            // Dismiss on focus loss (§5.2 step 3: blur is a dismissal path).
            if let Some(window) = app.get_webview_window("launcher") {
                let handle = app.handle().clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::Focused(false) = event {
                        let in_view = handle
                            .try_state::<ViewState>()
                            .is_some_and(|v| v.0.load(Ordering::SeqCst));
                        if !in_view {
                            dismiss(&handle);
                        }
                    }
                });
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running yspot-shell");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alt_space() -> settings::Hotkey {
        settings::Hotkey {
            ctrl: false,
            alt: true,
            shift: false,
            win: false,
            code: "Space".to_string(),
        }
    }

    #[test]
    fn a_conflict_is_reported_even_though_the_chord_itself_is_valid() {
        let chord = alt_space();
        // The premise of the bug: the static check passes, so it cannot be
        // the only thing onboarding asks.
        assert_eq!(chord.rejection(), None);
        let live = Some("Alt + Space could not be registered.".to_string());
        assert_eq!(hotkey_error(live.clone(), &chord), live);
    }

    fn with(source: settings::HotkeySource, hotkey: settings::Hotkey) -> Settings {
        Settings {
            hotkey,
            hotkey_source: source,
            ..Settings::default()
        }
    }

    #[test]
    fn the_registrar_decision_table() {
        use settings::HotkeySource::{Shell, Ykeys};
        let a = alt_space();
        let b = settings::Hotkey {
            ctrl: true,
            alt: false,
            ..alt_space()
        };

        // Nothing changed and the chord is held: leave the registrar alone.
        assert_eq!(
            registrar_action(&with(Shell, a.clone()), &with(Shell, a.clone()), false),
            Registrar::Nothing
        );
        // A different chord: §5.1's atomic replace.
        assert_eq!(
            registrar_action(&with(Shell, a.clone()), &with(Shell, b.clone()), false),
            Registrar::Replace
        );
        // The same chord, not held: §5.1's alternatives include chords a user
        // may already have, so clicking one has to actually try again.
        assert_eq!(
            registrar_action(&with(Shell, a.clone()), &with(Shell, a.clone()), true),
            Registrar::Retry
        );
        // Handing the chord to YKeys is a runtime act, contested or not — it
        // is the remedy for a contested chord, so it must not be refused then.
        assert_eq!(
            registrar_action(&with(Shell, a.clone()), &with(Ykeys, a.clone()), true),
            Registrar::HandToYkeys
        );
        assert_eq!(
            registrar_action(&with(Ykeys, a.clone()), &with(Ykeys, b.clone()), false),
            Registrar::HandToYkeys
        );
        // And taking it back binds now, not at the next restart.
        assert_eq!(
            registrar_action(&with(Ykeys, a.clone()), &with(Shell, a.clone()), false),
            Registrar::TakeBack
        );
        assert_eq!(
            registrar_action(&with(Ykeys, a), &with(Shell, b), false),
            Registrar::TakeBack
        );
    }

    fn request(gen: u64, text: &str) -> FallbackRequest {
        FallbackRequest {
            gen,
            text: text.to_string(),
            reason: "test".to_string(),
        }
    }

    #[test]
    fn a_burst_of_keystrokes_leaves_only_the_newest_query_to_run() {
        // "readme.md" used to put nine uncancellable OLE DB round trips on the
        // blocking pool at once, with the one the user was waiting on queued
        // behind eight already-dead ones.
        let q = FallbackQueue::default();
        for (i, text) in ["r", "re", "rea", "read"].iter().enumerate() {
            q.submit(request(i as u64 + 1, text));
        }
        let got = q.take();
        assert_eq!(got.gen, 4);
        assert_eq!(got.text, "read");
        // And nothing is left behind for a second worker pass.
        assert!(q.slot.lock().unwrap().is_none());
    }

    #[test]
    fn a_superseded_result_is_not_worth_emitting() {
        let q = FallbackQueue::default();
        q.submit(request(1, "r"));
        assert!(q.is_current(1));
        // A keystroke lands while gen 1 is inside the provider.
        q.submit(request(2, "re"));
        assert!(!q.is_current(1));
        assert!(q.is_current(2));
        // An out-of-order arrival must not un-supersede anything: `latest` only
        // ever moves forward.
        q.submit(request(1, "r"));
        assert!(!q.is_current(1));
        assert!(q.is_current(2));
    }

    #[test]
    fn file_queries_split_into_a_name_and_filters() {
        let (name, f) = parse_file_query("report kind:document path:projects ext:.PDF,md").unwrap();
        assert_eq!(name, "report");
        assert_eq!(f.path_substr.as_deref(), Some("projects"));
        // kind ∩ ext: the document set narrowed to the two extensions named,
        // dots and case normalised on the way.
        let mut ext = f.ext.clone();
        ext.sort();
        assert_eq!(ext, vec!["md".to_string(), "pdf".to_string()]);

        // Nothing to filter: everything is the name, including a token that
        // merely contains a colon.
        let (name, f) = parse_file_query("notes 12:30").unwrap();
        assert_eq!(name, "notes 12:30");
        assert!(f.ext.is_empty() && f.path_substr.is_none());

        // Refused by name rather than applied to nothing.
        assert!(parse_file_query("x kind:folder")
            .unwrap_err()
            .contains("folder"));
        assert!(parse_file_query("x kind:banana")
            .unwrap_err()
            .contains("banana"));
    }

    #[test]
    fn file_filters_combine_with_and_and_refuse_what_cannot_match() {
        // kind ∩ ext, not kind ∪ ext: the service ORs the list it is given.
        let (_, f) = parse_file_query("x kind:image ext:png,rs").unwrap();
        assert_eq!(f.ext, vec!["png".to_string()]);
        // Disjoint: nothing could match, and saying so beats an empty list.
        assert!(parse_file_query("x kind:image ext:rs")
            .unwrap_err()
            .contains("ext:"));
        // A prefix of a kind, mid-typing, is no filter yet rather than an error.
        let (name, f) = parse_file_query("x kind:im").unwrap();
        assert_eq!(name, "x");
        assert!(f.ext.is_empty());
        // Filters alone cannot list the index — including the forms that
        // set no filter yet, which would otherwise send an empty query.
        for q in [
            "kind:image",
            "path:src",
            "kind:",
            "kind:im",
            "ext:",
            "path:",
        ] {
            assert!(
                parse_file_query(q).unwrap_err().contains("add a word"),
                "{q:?}"
            );
        }
        // An ext still being typed under a kind is the kind alone, not a
        // refusal on every keystroke; a finished mismatch is refused.
        let (_, f) = parse_file_query("x kind:image ext:p").unwrap();
        assert!(f.ext.contains(&"png".to_string()) && f.ext.contains(&"jpg".to_string()));
        assert!(parse_file_query("x kind:image ext:zzz")
            .unwrap_err()
            .contains("ext:"));
        // A bare `path:` with a name is no filter yet, not a word.
        let (name, f) = parse_file_query("report path:").unwrap();
        assert_eq!(name, "report");
        assert!(f.path_substr.is_none());
        // A drive letter is a name, not a filter key.
        let (name, f) = parse_file_query(r"c:\users report").unwrap();
        assert_eq!(name, r"c:\users report");
        assert!(f.ext.is_empty());
    }

    #[test]
    fn a_chord_that_registered_reports_nothing() {
        assert_eq!(hotkey_error(None, &alt_space()), None);
    }

    #[test]
    fn a_malformed_chord_still_falls_back_to_the_static_reason() {
        // Never registered, so there is no live outcome to report — but the
        // shape is wrong and onboarding must still say so.
        let chord = settings::Hotkey {
            code: "AltLeft".to_string(),
            ..alt_space()
        };
        assert!(hotkey_error(None, &chord).is_some());
    }
}
