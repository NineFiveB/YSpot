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
mod diagnostics;
mod etw_mark;
mod file_actions;
mod focus;
mod frecency;
mod icons;
mod matcher;
mod pipe_client;
mod placement;
mod search_fallback;
mod settings;
mod settings_catalog;
mod tray;
mod winman;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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

/// Register `hotkey`, replacing whatever is bound now.
///
/// §5.1's atomicity rule: unregister the old chord, register the new one,
/// and if that fails put the old one back — a failed rebind must not leave
/// the launcher unreachable.
fn rebind_hotkey(
    app: &AppHandle,
    old: Option<&settings::Hotkey>,
    next: &settings::Hotkey,
) -> Result<(), String> {
    let shortcut = shortcut_of(next).ok_or_else(|| format!("unknown key {:?}", next.code))?;
    if let Some(reason) = next.rejection() {
        return Err(reason.to_string());
    }
    let manager = app.global_shortcut();
    let previous = old.and_then(shortcut_of);
    if let Some(p) = previous {
        let _ = manager.unregister(p);
    }
    match manager.register(shortcut) {
        Ok(()) => {
            log::info!("hotkey bound to {} (§5.1)", next.accelerator());
            Ok(())
        }
        Err(e) => {
            // Put the old chord back before reporting, so the launcher stays
            // reachable while the user picks another one.
            if let Some(p) = previous {
                let _ = manager.register(p);
            }
            let owner = next
                .likely_owner()
                .map(|o| format!(" It is probably held by {o}."))
                .unwrap_or_default();
            Err(format!(
                "{} could not be registered.{owner} ({e})",
                next.accelerator()
            ))
        }
    }
}

#[derive(Default)]
struct WarmState(AtomicBool);

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
    // §5.3: recompute placement on every show.
    match placement::compute_placement() {
        Some(p) => {
            if let Err(e) = window.set_size(tauri::PhysicalSize::new(p.width, p.height)) {
                log::warn!("set_size failed: {e}");
            }
            if let Err(e) = window.set_position(tauri::PhysicalPosition::new(p.x, p.y)) {
                log::warn!("set_position failed: {e}");
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

fn dismiss(app: &AppHandle) {
    let Some(window) = app.get_webview_window("launcher") else {
        return;
    };
    if !window.is_visible().unwrap_or(false) {
        return;
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
    focus::restore_foreground();
    etw_mark::mark("hidden");
    let _ = app.emit("window:hidden", ());
    // Best effort: stop in-flight work for the current generation (§4.3 Cancel).
    if let Some(pipe) = app.try_state::<Arc<PipeClient>>() {
        let _ = pipe.cancel_current();
    }
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
    log::debug!("search cmd: gen={gen} text={text:?}");
    // The shell's own answers first, and synchronously: matching a few
    // hundred names is microseconds and the calculator is a parse of one
    // line, so all of it fits the §2.5 shell-routing share and lands in the
    // same animation frame as the keystroke that asked for it.
    let mut apps = apps::match_query(&catalog.snapshot(), &text, apps::MAX_APP_RESULTS);
    for it in &mut apps {
        // §7.1: frecency reorders within a tier and can never lift a lower
        // tier above a higher one — the bonus is bounded under the tier gap.
        it.score += frec.bonus(&it.id);
    }
    apps.sort_by(|a, b| b.score.total_cmp(&a.score));

    let mut settings = settings.match_query(&text, settings_catalog::MAX_RESULTS);
    for it in &mut settings {
        it.score += frec.bonus(&it.id);
    }
    settings.sort_by(|a, b| b.score.total_cmp(&a.score));

    let mut builtin_hits = commands::match_query(&builtins, &text, commands::MAX_RESULTS);
    for it in &mut builtin_hits {
        it.score += frec.bonus(&it.id);
    }
    builtin_hits.sort_by(|a, b| b.score.total_cmp(&a.score));

    // Open windows (§7.5). The enumeration is cached, so the per-keystroke
    // cost is a match over a few dozen titles.
    let mut window_hits = winman::match_query(&win_cache.snapshot(), &text, winman::MAX_RESULTS);
    for it in &mut window_hits {
        it.score += frec.bonus(&it.id);
    }
    window_hits.sort_by(|a, b| b.score.total_cmp(&a.score));

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

/// §7.1 icon extraction, off the query path: the frontend asks per visible
/// row and renders a placeholder until this resolves (§5.10).
#[tauri::command]
async fn app_icon(
    cache: tauri::State<'_, Arc<IconCache>>,
    id: String,
    px: i32,
) -> Result<String, String> {
    let cache = cache.inner().clone();
    tauri::async_runtime::spawn_blocking(move || cache.app_icon(&id, px))
        .await
        .map_err(|e| format!("icon task: {e}"))?
        .map(|uri| uri.to_string())
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
        || (kind == "command" && matches!(id.as_str(), "yspot.settings" | "yspot.clipboard"));
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
            "yspot.clipboard" => show_view(&app, "view:clipboard").map(|()| false),
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

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct FallbackItem {
    path: String,
    name: String,
}

/// Run the shell-side file search for one generation and emit its results.
///
/// The single entry point §9.5 asks for: portable mode and §3.1's `107`
/// routing are the same code path, not two implementations.
pub(crate) fn run_fallback_search(app: &AppHandle, gen: u64, text: String, reason: &str) {
    let app = app.clone();
    let reason = reason.to_string();
    tauri::async_runtime::spawn_blocking(move || {
        let Some(provider) = app.try_state::<Arc<dyn FileSearch>>() else {
            return;
        };
        let payload = match provider.search(&text, 50) {
            Ok(hits) => FallbackResults {
                gen,
                items: hits
                    .into_iter()
                    .map(|h| FallbackItem {
                        path: h.path,
                        name: h.name,
                    })
                    .collect(),
                reason,
                unavailable: None,
            },
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
        if let Err(e) = app.emit("search:fallback", payload) {
            log::warn!("emit search:fallback failed: {e}");
        }
    });
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
    // Dismiss FIRST: the paste restores the previous foreground window, and
    // that cannot happen while the launcher still holds focus.
    dismiss(&app);
    clipboard::paste(&text)
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

#[tauri::command]
fn clipboard_set_enabled(clip: tauri::State<'_, Arc<ClipboardStore>>, enabled: bool) {
    clip.set_enabled(enabled);
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
}

#[tauri::command]
fn get_settings(store: tauri::State<'_, Arc<SettingsStore>>) -> SettingsView {
    let settings = store.get();
    SettingsView {
        hotkey_warning: settings.hotkey.warning().map(str::to_string),
        autostart: autostart::is_enabled(),
        settings,
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
    if next.hotkey != current.hotkey {
        rebind_hotkey(&app, Some(&current.hotkey), &next.hotkey)?;
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
    let Some(p) = placement::compute_placement_of_height(logical) else {
        return Err("placement computation failed".to_string());
    };
    window
        .set_size(tauri::PhysicalSize::new(p.width, p.height))
        .map_err(|e| format!("set_size: {e}"))?;
    window
        .set_position(tauri::PhysicalPosition::new(p.x, p.y))
        .map_err(|e| format!("set_position: {e}"))
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
    etw_mark::init();

    if autostart::started_hidden() {
        // §5.4 warm start: the window is created hidden either way (the
        // Tauri config marks it invisible), so the flag only records why.
        log::info!("started with --hidden (autostart): warming the renderer, staying invisible");
    }

    let store = Arc::new(SettingsStore::open());
    let clip = ClipboardStore::open();
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
        .invoke_handler(tauri::generate_handler![
            search,
            hide_window,
            frontend_ready,
            execute_action,
            app_icon,
            get_settings,
            save_settings,
            open_settings,
            clipboard_list,
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

            // `yspot.exe --settings` opens the Settings window directly, for
            // a shortcut or a script; the launcher itself stays hidden.
            if std::env::args().any(|a| a == "--settings") {
                if let Err(e) = show_settings(app.handle()) {
                    log::error!("could not open Settings: {e}");
                }
            }

            // §5.1: the binding persists in settings and re-registers on
            // every start.
            let hotkey = app.state::<Arc<SettingsStore>>().get().hotkey;
            if let Err(e) = rebind_hotkey(app.handle(), None, &hotkey) {
                log::error!(
                    "hotkey registration failed: {e} Rebind it in Settings \
                     (tray → Settings…, or the `settings` query)."
                );
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
