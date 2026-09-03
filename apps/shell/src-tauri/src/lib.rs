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
mod com;
mod etw_mark;
mod file_actions;
mod focus;
mod frecency;
mod icons;
mod pipe_client;
mod placement;
mod tray;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use apps::AppCatalog;
use frecency::Frecency;
use icons::IconCache;
use pipe_client::PipeClient;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

/// §5.1 default binding. The plugin does not expose MOD_NOREPEAT, so holding
/// the chord can retrigger the toggle — acceptable for M0, rebinding is M1.
fn alt_space() -> Shortcut {
    Shortcut::new(Some(Modifiers::ALT), Code::Space)
}

#[derive(Default)]
struct WarmState(AtomicBool);

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

/// One generation's app results (§5.11: a shell-side catalog source, on
/// screen in the same frame as the keystroke).
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct AppResults {
    gen: u64,
    items: Vec<apps::AppMatch>,
}

#[tauri::command]
fn search(
    app: AppHandle,
    pipe: tauri::State<'_, Arc<PipeClient>>,
    catalog: tauri::State<'_, Arc<AppCatalog>>,
    frec: tauri::State<'_, Arc<Frecency>>,
    gen: u64,
    text: String,
) -> Result<Accepted, String> {
    log::debug!("search cmd: gen={gen} text={text:?}");
    // Apps first, and synchronously: matching a few hundred names is
    // microseconds, and §5.11 expects catalog sources to land in ~1 ms —
    // before the service's first batch, in the same animation frame as the
    // keystroke that asked for them.
    let mut items = apps::match_query(&catalog.snapshot(), &text, apps::MAX_APP_RESULTS);
    for it in &mut items {
        // §7.1: frecency reorders within a tier and can never lift a lower
        // tier above a higher one — the bonus is bounded under the tier gap.
        it.score += frec.bonus(&it.id);
    }
    items.sort_by(|a, b| b.score.total_cmp(&a.score));
    log::debug!(
        "apps gen={gen}: {} match(es){}",
        items.len(),
        items
            .first()
            .map(|m| format!(", best {:?} {:.3}", m.name, m.score))
            .unwrap_or_default()
    );
    if let Err(e) = app.emit("search:apps", AppResults { gen, items }) {
        log::warn!("emit search:apps failed: {e}");
    }
    // The service half is enqueued after, so a slow pipe cannot delay apps.
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
fn execute_action(
    app: AppHandle,
    frec: tauri::State<'_, Arc<Frecency>>,
    catalog: tauri::State<'_, Arc<AppCatalog>>,
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
    let stays_open = matches!(action.as_str(), "copy_path" | "copy_file");
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

/// §5.4 autostart state, for the Settings UI (§5.9) and the tray toggle.
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
fn shell_open(path: &str) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    if path.is_empty() {
        return Err("empty path".to_string());
    }
    let path_w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `path_w` is a valid NUL-terminated UTF-16 string; null hwnd,
    // verb, parameters, and directory are all documented as permitted.
    let inst = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            std::ptr::null(),
            path_w.as_ptr(),
            std::ptr::null(),
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
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    etw_mark::init();

    if autostart::started_hidden() {
        // §5.4 warm start: the window is created hidden either way (the
        // Tauri config marks it invisible), so the flag only records why.
        log::info!("started with --hidden (autostart): warming the renderer, staying invisible");
    }

    let pipe = Arc::new(PipeClient::new());
    let catalog = AppCatalog::new();
    let frec = Frecency::open();
    let icon_cache = IconCache::new();

    tauri::Builder::default()
        // §5.5 single instance: a second launch forwards a show to the
        // running shell and exits, so running the exe again summons the
        // launcher instead of starting a rival that fights for the hotkey.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            log::info!("second instance launched; summoning this one (§5.5)");
            show(app);
        }))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    // Nothing on this path performs IPC or disk I/O (§5.1).
                    if matches!(event.state(), ShortcutState::Pressed) && shortcut == &alt_space() {
                        toggle(app);
                    }
                })
                .build(),
        )
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
            get_autostart,
            set_autostart,
            get_status,
            m0_mark,
            m0_report,
            m0_spec
        ])
        .setup(move |app| {
            pipe_client::spawn(app.handle().clone(), pipe.clone());
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

            match app.global_shortcut().register(alt_space()) {
                Ok(()) => log::info!("registered Alt+Space (RegisterHotKey via plugin, §5.1)"),
                Err(e) => log::error!(
                    "Alt+Space registration failed: {e}. Likely owners: PowerToys Run \
                     (Alt+Space), Copilot (Alt+Space on some Win11 builds). The conflict \
                     dialog and rebinding flow are M1 (§5.1)."
                ),
            }

            // Dismiss on focus loss (§5.2 step 3: blur is a dismissal path).
            if let Some(window) = app.get_webview_window("launcher") {
                let handle = app.handle().clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::Focused(false) = event {
                        dismiss(&handle);
                    }
                });
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running yspot-shell");
}
