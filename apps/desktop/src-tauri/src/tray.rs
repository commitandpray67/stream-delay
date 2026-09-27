//! Tray icon: colored by delay state, with presets and quick links in its menu.

use std::sync::{Arc, Mutex};

use streamdelay_control::App;
use streamdelay_relay::{DelayMode, EgressStatus, GoLiveWhen, Phase, RelayState};
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
    back: MenuItem<Wry>,
}

#[derive(Default)]
struct TrayHandles(Mutex<Option<TrayItems>>);

fn icon(phase: Phase) -> Image<'static> {
    let bytes: &'static [u8] = match phase {
        Phase::Live => include_bytes!("../icons/tray-live.png"),
        Phase::Delayed => include_bytes!("../icons/tray-delayed.png"),
        Phase::Adding | Phase::GoingLive | Phase::Reducing | Phase::Holding => {
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
        Phase::Holding => "Holding the picture…".into(),
    }
}

/// The delay set, when an outage made the delay longer (see
/// `Snapshot::excess_ms`): the tray offers to go back to it.
fn back_to_ms(state: &RelayState) -> Option<u64> {
    let d = &state.delay;
    (d.excess_ms > 0 && d.target_ms > 0 && !state.ended).then_some(d.target_ms)
}

fn back_text(state: &RelayState) -> String {
    match back_to_ms(state) {
        Some(ms) => format!(
            "Back to {} s (the delay grew {} s after a connection problem)",
            (ms as f64 / 1000.0).round(),
            (state.delay.excess_ms as f64 / 1000.0).round()
        ),
        None => "Back to the delay set".into(),
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
    let back = MenuItem::with_id(
        app,
        "back",
        back_text(&state),
        back_to_ms(&state).is_some(),
        None::<&str>,
    )?;
    menu.append(&back)?;
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
    Ok((menu, TrayItems { status, back }))
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
        let mut last: Option<(Phase, String, String)> = None;
        while rx.changed().await.is_ok() {
            let state = rx.borrow_and_update().clone();
            let text = status_text(&state);
            let back = back_text(&state);
            let key = (state.delay.phase, text.clone(), back.clone());
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
                let _ = items.back.set_text(&back);
                let _ = items.back.set_enabled(back_to_ms(&state).is_some());
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
        "back" => {
            if let Some(ms) = back_to_ms(&core.relay().state()) {
                tauri::async_runtime::spawn(async move {
                    // A reduction: it skips ahead at the next keyframe old
                    // enough, never lower.
                    if let Err(e) = core.relay().set_delay(ms, DelayMode::Rewind).await {
                        warn!("set delay failed: {e}");
                    }
                });
            }
        }
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
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let r = core.dump().await;
                crate::dump_notice(&app, &r);
                if let Err(e) = r {
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

/// The question offering version `version`: message, install and later
/// buttons. Like quitting, while streaming it says what installing does to the
/// stream.
fn update_prompt(version: &str, live: bool) -> (String, &'static str, &'static str) {
    if live {
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
    }
}

/// Whether an answer of Install, to a question asked while streaming (`warned`)
/// or not, may install now that the app is streaming (`live`) or not. The
/// question can stay up for a long time: one asked before a stream started
/// did not say that installing ends it, and is asked again.
fn install_now(warned: bool, live: bool) -> bool {
    warned || !live
}

/// Asks whether to install `update`, and installs it if so.
fn offer_update(app: AppHandle, update: tauri_plugin_updater::Update) {
    let live = app.try_state::<Core>().is_some_and(|c| streaming(&c.0));
    let (message, install, later) = update_prompt(&update.version, live);
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
        if !install {
            return;
        }
        let live_now = app2.try_state::<Core>().is_some_and(|c| streaming(&c.0));
        if install_now(live, live_now) {
            tauri::async_runtime::spawn(install_update(app2, update));
        } else {
            offer_update(app2, update);
        }
    });
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
            Ok(Some(update)) => offer_update(app, update),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tray_offers_to_go_back_only_after_an_outage_stretched_the_delay() {
        let mut state = RelayState::default();
        state.delay.phase = Phase::Delayed;
        state.delay.target_ms = 30_000;
        state.delay.effective_ms = 31_900;
        assert_eq!(back_to_ms(&state), None);
        state.delay.effective_ms = 52_000;
        state.delay.excess_ms = 22_000;
        assert_eq!(back_to_ms(&state), Some(30_000));
        assert!(back_text(&state).starts_with("Back to 30 s (the delay grew 22 s"));
        assert_eq!(status_text(&state), "Delayed 52 s");
        state.ended = true;
        assert_eq!(back_to_ms(&state), None);
    }

    #[test]
    fn installing_asks_again_if_a_stream_started_while_the_question_was_up() {
        // Asked while not streaming, answered while streaming: that answer was
        // given without knowing that installing ends the stream.
        assert!(!install_now(false, true));
        assert!(install_now(false, false));
        assert!(install_now(true, true));
        assert!(install_now(true, false), "the stream ended meanwhile");
        let (message, install, later) = update_prompt("9.9.9", true);
        assert!(message.contains("ends your stream now"), "{message}");
        assert_eq!(
            (install, later),
            ("Install and end the stream", "Keep streaming")
        );
        let (message, install, _) = update_prompt("9.9.9", false);
        assert!(!message.contains("stream now"), "{message}");
        assert_eq!(install, "Install");
    }
}
