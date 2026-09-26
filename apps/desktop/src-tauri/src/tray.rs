//! Tray icon: colored by delay state, with presets and quick links in its menu.

use std::sync::{Arc, Mutex};

use streamdelay_control::App;
use streamdelay_relay::{EgressStatus, GoLiveWhen, Phase, RelayState};
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
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
    if state.ended {
        return "Stream ended: nothing is being sent".into();
    }
    if state.ending {
        return "Ending the stream once the buffer has aired…".into();
    }
    match d.phase {
        Phase::Offline => "Offline: waiting for OBS".into(),
        Phase::Live => "No delay".into(),
        Phase::Delayed => format!("Delayed {secs} s"),
        Phase::Adding => "Adding delay…".into(),
        Phase::GoingLive => "Removing delay…".into(),
        Phase::Reducing => "Changing delay…".into(),
    }
}

fn preset_label(seconds: f64) -> String {
    if seconds <= 0.0 {
        "No delay".into()
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
        "Remove the delay once the buffer has aired",
        true,
        None::<&str>,
    )?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(
        app,
        "dump",
        "Dump the buffer (what has not aired never does)",
        true,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(
        app,
        "end-after-air",
        "End stream once the buffer has aired",
        true,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(
        app,
        "end-stream",
        "End stream now (the buffer never airs)",
        true,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(
        app,
        "resume",
        "Resume broadcasting",
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
        "end-stream" => {
            tauri::async_runtime::spawn(async move {
                if let Err(e) = core.relay().end_stream().await {
                    warn!("end stream failed: {e}");
                }
            });
        }
        "end-after-air" => {
            tauri::async_runtime::spawn(async move {
                if let Err(e) = core.relay().end_stream_after_air().await {
                    warn!("end stream failed: {e}");
                }
            });
        }
        "dump" => {
            tauri::async_runtime::spawn(async move {
                if let Err(e) = core.dump().await {
                    warn!("dump failed: {e}");
                }
            });
        }
        "resume" => {
            tauri::async_runtime::spawn(async move {
                if let Err(e) = core.relay().resume().await {
                    warn!("resume failed: {e}");
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
            if !streaming(&core) {
                info!("quitting from the tray");
                app.exit(0);
                return;
            }
            // Quitting ends the stream: make sure that is what was meant.
            let app2 = app.clone();
            app.dialog()
                .message(
                    "You are streaming through stream-delay. Quitting ends your stream now, \
                     and what is still in the delay buffer does not air.",
                )
                .title("Quit stream-delay?")
                .kind(MessageDialogKind::Warning)
                .buttons(MessageDialogButtons::OkCancelCustom(
                    "Quit and end the stream".into(),
                    "Keep streaming".into(),
                ))
                .show(move |quit| {
                    if quit {
                        info!("quitting from the tray while streaming");
                        app2.exit(0);
                    }
                });
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

/// True while OBS streams to stream-delay or the destination is live: quitting
/// or updating then ends the stream.
fn streaming(core: &App) -> bool {
    let state = core.relay().state();
    state.ingest.connected || state.egress.status == EgressStatus::Live
}

fn copy(app: &AppHandle, text: String) {
    if let Err(e) = app.clipboard().write_text(text) {
        warn!("clipboard: {e}");
    }
}

/// Downloads and installs `update`, then restarts. The stream is ended cleanly
/// before installing, as quitting does: on Windows, installing exits the app at
/// once, without its shutdown, and the broadcast would just be cut off.
async fn install_update(app: AppHandle, update: tauri_plugin_updater::Update) {
    let bytes = match update.download(|_, _| {}, || {}).await {
        Ok(bytes) => bytes,
        Err(e) => {
            warn!("update download failed: {e}");
            app.dialog()
                .message(format!("Could not download the update: {e}"))
                .title("stream-delay")
                .show(|_| {});
            return;
        }
    };
    if let Some(core) = app.try_state::<Core>() {
        info!("stopping the relay to install the update");
        core.0.shutdown().await;
    }
    if let Err(e) = update.install(bytes) {
        warn!("update failed: {e}");
        // The relay has stopped: start again, as this version.
        let app2 = app.clone();
        app.dialog()
            .message(format!(
                "Could not install the update: {e}. stream-delay restarts."
            ))
            .title("stream-delay")
            .kind(MessageDialogKind::Error)
            .show(move |_| app2.restart());
        return;
    }
    app.restart();
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
                let live = app.try_state::<Core>().is_some_and(|c| streaming(&c.0));
                // Like quitting: while live, say what installing does to the stream.
                let (message, install, later) = if live {
                    (
                        format!(
                            "stream-delay {version} is available. You are streaming through \
                             stream-delay: installing it ends your stream now, and what is still \
                             in the delay buffer does not air. The app restarts afterwards."
                        ),
                        "Install and end the stream",
                        "Keep streaming",
                    )
                } else {
                    (
                        format!(
                            "stream-delay {version} is available. Install it now? The app \
                             restarts afterwards."
                        ),
                        "Install",
                        "Later",
                    )
                };
                let app2 = app.clone();
                let mut dialog = app
                    .dialog()
                    .message(message)
                    .title("Update available")
                    .buttons(MessageDialogButtons::OkCancelCustom(
                        install.into(),
                        later.into(),
                    ));
                if live {
                    dialog = dialog.kind(MessageDialogKind::Warning);
                }
                dialog.show(move |install| {
                    if install {
                        tauri::async_runtime::spawn(install_update(app2, update));
                    }
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
