//! Tray icon: colored by delay state, with presets and quick links in its menu.

use std::sync::{Arc, Mutex};

use streamdelay_control::App;
use streamdelay_relay::{GoLiveWhen, Phase, RelayState};
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_updater::UpdaterExt;
use tracing::{info, warn};

use crate::{Core, show_dashboard};

const TRAY_ID: &str = "main";

/// Menu items updated at runtime.
struct TrayItems {
    status: MenuItem<Wry>,
}

#[derive(Default)]
struct TrayHandles(Mutex<Option<TrayItems>>);

fn icon(phase: Phase) -> Image<'static> {
    let bytes: &'static [u8] = match phase {
        Phase::Live => include_bytes!("../icons/tray-live.png"),
        Phase::Delayed => include_bytes!("../icons/tray-delayed.png"),
        Phase::Adding | Phase::GoingLive | Phase::Reducing => {
            include_bytes!("../icons/tray-busy.png")
        }
        Phase::Offline => include_bytes!("../icons/tray-offline.png"),
    };
    Image::from_bytes(bytes).expect("bundled tray icons are valid PNGs")
}

fn status_text(state: &RelayState) -> String {
    let d = &state.delay;
    let secs = (d.effective_ms as f64 / 1000.0).round();
    match d.phase {
        Phase::Offline => "Offline: waiting for OBS".into(),
        Phase::Live => "Live (no delay)".into(),
        Phase::Delayed => format!("Delayed {secs} s"),
        Phase::Adding => "Adding delay…".into(),
        Phase::GoingLive => "Going live…".into(),
        Phase::Reducing => "Changing delay…".into(),
    }
}

fn preset_label(seconds: f64) -> String {
    if seconds <= 0.0 {
        "Go live now".into()
    } else if seconds >= 60.0 && seconds % 60.0 == 0.0 {
        format!("Delay {} min", seconds / 60.0)
    } else {
        format!("Delay {seconds} s")
    }
}

fn build_menu(app: &AppHandle, core: &App) -> tauri::Result<(Menu<Wry>, TrayItems)> {
    let state = core.relay().state();
    let status = MenuItem::with_id(app, "status", status_text(&state), false, None::<&str>)?;
    let menu = Menu::new(app)?;
    menu.append(&status)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    for (i, p) in core.config().delay.presets.iter().enumerate() {
        let item = MenuItem::with_id(
            app,
            format!("preset:{i}"),
            preset_label(p.seconds),
            true,
            None::<&str>,
        )?;
        menu.append(&item)?;
    }
    menu.append(&MenuItem::with_id(
        app,
        "after-air",
        "Go live after it airs",
        true,
        None::<&str>,
    )?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(
        app,
        "dashboard",
        "Open dashboard…",
        true,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(
        app,
        "setup",
        "Set up OBS…",
        true,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(
        app,
        "copy-server",
        "Copy OBS server address",
        true,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(
        app,
        "copy-dock",
        "Copy OBS dock URL",
        true,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(
        app,
        "copy-overlay",
        "Copy overlay URL",
        true,
        None::<&str>,
    )?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    let autostart = app.autolaunch().is_enabled().unwrap_or(false);
    menu.append(&CheckMenuItem::with_id(
        app,
        "autostart",
        "Start with my computer",
        true,
        autostart,
        None::<&str>,
    )?)?;
    if crate::updater_configured(app) {
        menu.append(&MenuItem::with_id(
            app,
            "updates",
            "Check for updates…",
            true,
            None::<&str>,
        )?)?;
    }
    menu.append(&MenuItem::with_id(
        app,
        "quit",
        "Quit stream-delay",
        true,
        None::<&str>,
    )?)?;
    Ok((menu, TrayItems { status }))
}

pub fn create(app: &AppHandle, core: &Arc<App>) -> tauri::Result<()> {
    app.manage(TrayHandles::default());
    let (menu, items) = build_menu(app, core)?;
    *app.state::<TrayHandles>().0.lock().expect("tray lock") = Some(items);
    let state = core.relay().state();
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon(state.delay.phase))
        .tooltip(format!("stream-delay: {}", status_text(&state)))
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| on_menu(app, event.id().as_ref()))
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_dashboard(tray.app_handle(), None);
            }
        })
        .build(app)?;
    follow_state(app.clone(), core.clone());
    Ok(())
}

pub fn rebuild_menu(app: &AppHandle, core: &App) -> tauri::Result<()> {
    let (menu, items) = build_menu(app, core)?;
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        tray.set_menu(Some(menu))?;
    }
    *app.state::<TrayHandles>().0.lock().expect("tray lock") = Some(items);
    Ok(())
}

/// Keeps the icon, tooltip and status line in sync with the relay.
fn follow_state(app: AppHandle, core: Arc<App>) {
    let mut rx = core.relay().subscribe();
    tauri::async_runtime::spawn(async move {
        let mut last: Option<(Phase, String)> = None;
        while rx.changed().await.is_ok() {
            let state = rx.borrow_and_update().clone();
            let text = status_text(&state);
            let key = (state.delay.phase, text.clone());
            if last.as_ref() == Some(&key) {
                continue;
            }
            last = Some(key);
            if let Some(tray) = app.tray_by_id(TRAY_ID) {
                update_tray(&tray, state.delay.phase, &text);
            }
            if let Some(items) = app
                .state::<TrayHandles>()
                .0
                .lock()
                .expect("tray lock")
                .as_ref()
            {
                let _ = items.status.set_text(&text);
            }
        }
    });
}

fn update_tray(tray: &TrayIcon<Wry>, phase: Phase, text: &str) {
    let _ = tray.set_icon(Some(icon(phase)));
    let _ = tray.set_tooltip(Some(format!("stream-delay: {text}")));
}

fn on_menu(app: &AppHandle, id: &str) {
    let Some(core) = app.try_state::<Core>().map(|c| c.0.clone()) else {
        return;
    };
    match id {
        "dashboard" => show_dashboard(app, None),
        "setup" => show_dashboard(app, Some("setup")),
        "copy-server" => copy(app, core.urls().obs_server),
        "copy-dock" => copy(app, core.urls().dock),
        "copy-overlay" => copy(app, core.urls().overlay),
        "after-air" => {
            tauri::async_runtime::spawn(async move {
                if let Err(e) = core.relay().go_live(GoLiveWhen::AfterAir).await {
                    warn!("go live failed: {e}");
                }
            });
        }
        "autostart" => {
            let auto = app.autolaunch();
            let result = if auto.is_enabled().unwrap_or(false) {
                auto.disable()
            } else {
                auto.enable()
            };
            if let Err(e) = result {
                warn!("could not change autostart: {e}");
            }
            let _ = rebuild_menu(app, &core);
        }
        "updates" => check_for_updates(app.clone(), true),
        "quit" => {
            info!("quitting from the tray");
            app.exit(0);
        }
        other => {
            if let Some(i) = other
                .strip_prefix("preset:")
                .and_then(|i| i.parse::<usize>().ok())
            {
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = core.apply_preset(i).await {
                        warn!("preset failed: {e}");
                    }
                });
            }
        }
    }
}

fn copy(app: &AppHandle, text: String) {
    if let Err(e) = app.clipboard().write_text(text) {
        warn!("clipboard: {e}");
    }
}

/// Checks GitHub Releases for a signed update. `interactive` reports "up to date"
/// and errors too; background checks only speak up when an update exists.
pub fn check_for_updates(app: AppHandle, interactive: bool) {
    tauri::async_runtime::spawn(async move {
        let result = async {
            let updater = app.updater()?;
            updater.check().await
        }
        .await;
        match result {
            Ok(Some(update)) => {
                let version = update.version.clone();
                let app2 = app.clone();
                app.dialog()
                    .message(format!(
                        "stream-delay {version} is available. Install it now? The app restarts afterwards; \
                         don't do this while you are live."
                    ))
                    .title("Update available")
                    .buttons(tauri_plugin_dialog::MessageDialogButtons::OkCancelCustom(
                        "Install".into(),
                        "Later".into(),
                    ))
                    .show(move |install| {
                        if !install {
                            return;
                        }
                        tauri::async_runtime::spawn(async move {
                            match update.download_and_install(|_, _| {}, || {}).await {
                                Ok(()) => app2.restart(),
                                Err(e) => warn!("update failed: {e}"),
                            }
                        });
                    });
            }
            Ok(None) if interactive => {
                app.dialog()
                    .message("You have the latest version.")
                    .title("stream-delay")
                    .show(|_| {});
            }
            Ok(None) => {}
            Err(e) => {
                warn!("update check failed: {e}");
                if interactive {
                    app.dialog()
                        .message(format!("Could not check for updates: {e}"))
                        .title("stream-delay")
                        .show(|_| {});
                }
            }
        }
    });
}
