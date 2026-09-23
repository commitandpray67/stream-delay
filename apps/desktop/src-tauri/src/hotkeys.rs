//! Global hotkeys (work while a game has focus). Configured under `[hotkeys]`.

use streamdelay_config::Config;
use streamdelay_relay::GoLiveWhen;
use tauri::{AppHandle, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};
use tracing::{info, warn};

use crate::Core;

#[derive(Debug, Clone, Copy)]
enum Action {
    GoLive,
    GoLiveAfterAir,
    EndStream,
    Preset(usize),
}

/// Replaces all registered shortcuts with the ones in `config`.
pub fn register(app: &AppHandle, config: &Config) {
    let shortcuts = app.global_shortcut();
    if let Err(e) = shortcuts.unregister_all() {
        warn!("could not clear hotkeys: {e}");
    }
    let h = &config.hotkeys;
    if !h.enabled {
        return;
    }
    let mut bindings = vec![
        (h.go_live.clone(), Action::GoLive),
        (h.go_live_after_air.clone(), Action::GoLiveAfterAir),
        (h.end_stream.clone(), Action::EndStream),
    ];
    let presets = config.delay.presets.len();
    bindings.extend(
        h.presets
            .iter()
            .take(presets)
            .enumerate()
            .map(|(i, s)| (s.clone(), Action::Preset(i))),
    );
    for (spec, action) in bindings {
        let spec = spec.trim().to_string();
        if spec.is_empty() {
            continue;
        }
        let result = shortcuts.on_shortcut(spec.as_str(), move |app, _shortcut, event| {
            if event.state == ShortcutState::Pressed {
                run(app, action);
            }
        });
        match result {
            Ok(()) => info!(hotkey = %spec, ?action, "hotkey registered"),
            // Usually another app owns the combination, or (on Wayland) global
            // shortcuts are not available; the dock and tray still work.
            Err(e) => warn!(hotkey = %spec, "could not register hotkey: {e}"),
        }
    }
}

fn run(app: &AppHandle, action: Action) {
    let Some(core) = app.try_state::<Core>().map(|c| c.0.clone()) else {
        return;
    };
    tauri::async_runtime::spawn(async move {
        let result = match action {
            Action::GoLive => core.relay().go_live(GoLiveWhen::Now).await.map(|_| ()),
            Action::GoLiveAfterAir => core.relay().go_live(GoLiveWhen::AfterAir).await.map(|_| ()),
            Action::EndStream => core.relay().end_stream().await,
            Action::Preset(i) => core.apply_preset(i).await,
        };
        if let Err(e) = result {
            warn!(?action, "hotkey action failed: {e}");
        }
    });
}
