//! Tray icon and its menu (SPEC.md §5.5).
//!
//! Present for as long as the shell runs, because a launcher with no window
//! and a hotkey someone else stole is otherwise unreachable. Left click
//! toggles the launcher; the context menu carries Open, Pause/Resume
//! indexing (§4.3 `PauseIndexing`, machine-wide and logged by the service),
//! Start with Windows (§5.4), and Quit.
//!
//! Quit exits the shell only. The indexing service keeps running: its
//! lifecycle belongs to Settings and the service manager, not to a tray menu
//! (§5.5).
//!
//! Deviation from §5.5's menu list, recorded in `docs/M1.md`: there is no
//! Settings… entry yet because the Settings window is Phase 2, and "Start
//! with Windows" stands in for the autostart toggle that §5.9 puts in
//! Settings and onboarding. Both move when those land.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tauri::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};

use crate::autostart;
use crate::pipe_client::PipeClient;

const ID_OPEN: &str = "open";
const ID_PAUSE: &str = "pause";
const ID_AUTOSTART: &str = "autostart";
const ID_QUIT: &str = "quit";

/// Whether indexing is currently paused *by us* — the tray label's state.
/// The service is the authority (`IndexStatus.state`), but the tray needs a
/// label before any status reply arrives, and a stale label self-corrects on
/// the next toggle.
#[derive(Default)]
pub struct PauseState(AtomicBool);

pub fn build(app: &AppHandle) -> tauri::Result<()> {
    let paused = Arc::new(PauseState::default());
    app.manage(paused.clone());

    let open = MenuItem::with_id(app, ID_OPEN, "Open YSpot", true, None::<&str>)?;
    let pause = MenuItem::with_id(app, ID_PAUSE, "Pause indexing", true, None::<&str>)?;
    let autostart_item = CheckMenuItem::with_id(
        app,
        ID_AUTOSTART,
        "Start with Windows",
        true,
        autostart::is_enabled(),
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, ID_QUIT, "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &open,
            &PredefinedMenuItem::separator(app)?,
            &pause,
            &autostart_item,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;

    let handle = app.clone();
    TrayIconBuilder::with_id("yspot")
        .icon(app.default_window_icon().cloned().ok_or_else(|| {
            tauri::Error::AssetNotFound("default window icon (bundle icon)".into())
        })?)
        .tooltip("YSpot")
        .menu(&menu)
        // The menu is the right-click surface only; a left click toggles the
        // launcher instead of opening it (§5.5).
        .show_menu_on_left_click(false)
        .on_tray_icon_event(move |tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                crate::toggle(tray.app_handle());
            }
        })
        .on_menu_event(move |app, event| on_menu(app, &handle, event, &pause, &autostart_item))
        .build(app)?;
    // Worth a line: the tray is the shell's only always-available surface,
    // and "no icon appeared" is otherwise indistinguishable from a crash.
    log::info!(
        "tray icon created (§5.5); autostart is currently {}",
        if autostart::is_enabled() { "on" } else { "off" }
    );
    Ok(())
}

fn on_menu(
    app: &AppHandle,
    _handle: &AppHandle,
    event: MenuEvent,
    pause_item: &MenuItem<Wry>,
    autostart_item: &CheckMenuItem<Wry>,
) {
    match event.id().as_ref() {
        ID_OPEN => crate::show(app),
        ID_PAUSE => {
            let Some(state) = app.try_state::<Arc<PauseState>>() else {
                return;
            };
            let Some(pipe) = app.try_state::<Arc<PipeClient>>() else {
                return;
            };
            let now_paused = !state.0.load(Ordering::SeqCst);
            let sent = if now_paused {
                pipe.pause_indexing()
            } else {
                pipe.resume_indexing()
            };
            match sent {
                Ok(()) => {
                    state.0.store(now_paused, Ordering::SeqCst);
                    let label = if now_paused {
                        "Resume indexing"
                    } else {
                        "Pause indexing"
                    };
                    if let Err(e) = pause_item.set_text(label) {
                        log::warn!("tray: set pause label: {e}");
                    }
                }
                // The service is the thing that pauses; if the request could
                // not be sent, the label must not claim it happened.
                Err(e) => log::warn!("tray: pause/resume not sent: {e}"),
            }
        }
        ID_AUTOSTART => {
            let want = !autostart::is_enabled();
            let result = if want {
                autostart::enable()
            } else {
                autostart::disable()
            };
            if let Err(e) = result {
                log::warn!("tray: autostart toggle failed: {e}");
            }
            // Read the registry back rather than trusting the click: the tick
            // then shows what is actually true, including after a failure.
            if let Err(e) = autostart_item.set_checked(autostart::is_enabled()) {
                log::warn!("tray: set autostart check: {e}");
            }
        }
        ID_QUIT => {
            log::info!("quit from tray; the indexing service keeps running (§5.5)");
            app.exit(0);
        }
        other => log::debug!("tray: unhandled menu id {other}"),
    }
}
