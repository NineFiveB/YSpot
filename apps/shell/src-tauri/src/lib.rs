//! YSpot shell — Tauri v2 host (SPEC.md §5), M0 subset:
//! Alt+Space toggle via the global-shortcut plugin (RegisterHotKey on
//! Windows, §5.1), §5.3 placement, §5.2 focus model, indexd pipe client,
//! and the §4.6 command surface.
//!
//! Deferred to M1: single-instance mutex + show forwarding (§5.5), tray
//! icon, hotkey conflict dialog and rebinding UI (§5.1), autostart (§5.4).

mod etw_mark;
mod focus;
mod pipe_client;
mod placement;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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

fn toggle(app: &AppHandle) {
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

fn show(app: &AppHandle) {
    let Some(window) = app.get_webview_window("launcher") else {
        return;
    };
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

#[tauri::command]
fn search(
    pipe: tauri::State<'_, Arc<PipeClient>>,
    gen: u64,
    text: String,
) -> Result<Accepted, String> {
    pipe.search(gen, text)?;
    Ok(Accepted { accepted: true })
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

#[tauri::command]
fn execute_action(app: AppHandle, path: String) -> Result<(), String> {
    shell_open(&path)?;
    dismiss(&app);
    Ok(())
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

    let pipe = Arc::new(PipeClient::new());

    tauri::Builder::default()
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
        .manage(WarmState::default())
        .invoke_handler(tauri::generate_handler![
            search,
            hide_window,
            frontend_ready,
            execute_action,
            get_status,
            m0_mark
        ])
        .setup(move |app| {
            pipe_client::spawn(app.handle().clone(), pipe.clone());

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

            // Single-instance enforcement (§5.5 CreateMutexW + show forwarding)
            // is deferred to M1.
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running yspot-shell");
}
