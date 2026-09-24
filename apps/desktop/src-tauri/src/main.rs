//! stream-delay desktop app.
//!
//! Runs the relay and control server in-process, shows a tray icon whose color
//! follows the delay state, opens the dashboard (the same web UI OBS docks use) in a
//! native window, and registers global hotkeys.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod hotkeys;
mod tray;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use streamdelay_config::{Config, Secrets};
use streamdelay_control::{App, AppError, AppOptions, Overrides};
use streamdelay_relay::RelayError;
use tauri::webview::DownloadEvent;
use tauri::{AppHandle, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tracing::{error, info, warn};
use tracing_subscriber::prelude::*;

/// The running core, shared with tray and hotkey handlers.
pub struct Core(pub Arc<App>);

/// Ports tried in order when the configured ones are taken (another RTMP server,
/// or a second copy of stream-delay).
const INGEST_FALLBACKS: [u16; 3] = [1935, 19350, 29350];
const API_FALLBACKS: [u16; 3] = [7788, 17788, 27788];

fn main() {
    init_logging();
    let minimized = std::env::args().any(|a| a == "--minimized");

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // A second launch just brings up the dashboard of the running one.
            show_dashboard(app, None);
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--minimized"]),
        ))
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .setup(move |app| {
            let handle = app.handle().clone();
            let core = match tauri::async_runtime::block_on(start_core()) {
                Ok(core) => Arc::new(core),
                Err(e) => {
                    error!("could not start: {e}");
                    handle
                        .dialog()
                        .message(format!("stream-delay could not start:\n\n{e}"))
                        .kind(MessageDialogKind::Error)
                        .title("stream-delay")
                        .blocking_show();
                    std::process::exit(1);
                }
            };
            app.manage(Core(core.clone()));
            tray::create(&handle, &core)?;
            hotkeys::register(&handle, &core.config());
            watch_config(handle.clone(), core.clone());
            if !minimized {
                let tab = core.needs_setup().then_some("setup");
                show_dashboard(&handle, tab);
            }
            if updater_configured(&handle) {
                tray::check_for_updates(handle.clone(), false);
                // The dashboard's "Check for updates" button.
                let updates = handle.clone();
                core.on_update_check(move || tray::check_for_updates(updates.clone(), true));
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build the stream-delay app")
        .run(|app, event| {
            // Closing the dashboard keeps the relay running in the tray; only the
            // tray's Quit (which calls `exit`) ends the app.
            if let RunEvent::ExitRequested { api, code, .. } = &event {
                if code.is_none() {
                    api.prevent_exit();
                }
            } else if let RunEvent::Exit = event
                && let Some(core) = app.try_state::<Core>()
            {
                let core = core.0.clone();
                tauri::async_runtime::block_on(async move { core.shutdown().await });
            }
        });
}

/// Largest log file. Past it the log starts again, keeping the full one as
/// `stream-delay.1.log`, so a long run, or someone flooding the log from the
/// network, cannot fill the disk.
const MAX_LOG_BYTES: u64 = 16 * 1024 * 1024;

/// The log file, started again when it reaches `max` bytes.
struct LogFile {
    path: PathBuf,
    file: std::fs::File,
    size: u64,
    max: u64,
}

impl LogFile {
    fn create(path: PathBuf, max: u64) -> std::io::Result<Self> {
        let file = std::fs::File::create(&path)?;
        Ok(Self {
            path,
            file,
            size: 0,
            max,
        })
    }
}

impl std::io::Write for LogFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.size > 0 && self.size + buf.len() as u64 > self.max {
            let _ = std::fs::rename(&self.path, self.path.with_file_name("stream-delay.1.log"));
            self.file = std::fs::File::create(&self.path)?;
            self.size = 0;
        }
        let n = self.file.write(buf)?;
        self.size += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,obws=error".into());
    let dir = Config::default_path()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("logs")));
    let file = dir.and_then(|d| {
        std::fs::create_dir_all(&d).ok()?;
        let path = d.join("stream-delay.log");
        // Keep the previous run's log: after a crash, that is the one that matters.
        let _ = std::fs::rename(&path, d.join("stream-delay.previous.log"));
        LogFile::create(path, MAX_LOG_BYTES).ok()
    });
    let output = match file {
        Some(f) => tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(f))
            .boxed(),
        None => tracing_subscriber::fmt::layer().boxed(),
    };
    tracing_subscriber::registry()
        .with(filter)
        .with(output)
        .with(streamdelay_control::diagnostics::layer())
        .init();
}

/// Starts relay and control server, moving to fallback ports if the configured
/// ones are in use (and remembering the ports that worked).
async fn start_core() -> Result<App, AppError> {
    let path = Config::default_path()?;
    let dir = path.parent().map(PathBuf::from).unwrap_or_default();
    let secrets = Arc::new(Secrets::new(&dir, true));
    let mut last_err = None;
    // Each failure moves one of the two ports, so this bounds the retries.
    for _ in 0..INGEST_FALLBACKS.len() + API_FALLBACKS.len() {
        let opts = AppOptions {
            config_path: Some(path.clone()),
            secrets: secrets.clone(),
            overrides: Overrides::default(),
        };
        match App::start(opts).await {
            Ok(app) => return Ok(app),
            Err(e) => {
                let mut config = Config::load_or_create(&path)?;
                // Moving to other ports would save them in the settings and break
                // the OBS server address and dock link when the port is only taken
                // by another copy of stream-delay.
                if matches!(
                    e,
                    AppError::Relay(RelayError::Bind { .. }) | AppError::Bind { .. }
                ) && stream_delay_answers(config.api.bind.port())
                {
                    return Err(AppError::AlreadyRunning);
                }
                let moved = match &e {
                    AppError::Relay(RelayError::Bind { addr, .. }) => {
                        next_port(&mut config.ingest.bind, *addr, &INGEST_FALLBACKS)
                    }
                    AppError::Bind { addr, .. } => {
                        next_port(&mut config.api.bind, *addr, &API_FALLBACKS)
                    }
                    _ => false,
                };
                if !moved {
                    return Err(e);
                }
                warn!("{e}; trying another port");
                config.save(&path)?;
                last_err = Some(e);
            }
        }
    }
    Err(last_err.expect("at least one attempt"))
}

/// True when stream-delay answers on `port` (its health check names the app).
fn stream_delay_answers(port: u16) -> bool {
    use std::io::{Read, Write};
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut s) = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(1)));
    let request = format!("GET /healthz HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n\r\n");
    if s.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut response = String::new();
    let _ = s.read_to_string(&mut response);
    response.contains(r#""app":"stream-delay""#)
}

/// Moves `bind` to the candidate after the one that failed. Returns false when
/// there is nothing left to try.
fn next_port(bind: &mut SocketAddr, failed: SocketAddr, candidates: &[u16]) -> bool {
    let next = match candidates.iter().position(|p| *p == failed.port()) {
        Some(i) => candidates.get(i + 1).copied(),
        None => candidates.first().copied(),
    };
    match next {
        Some(p) => {
            bind.set_port(p);
            true
        }
        None => false,
    }
}

/// Re-applies hotkeys and the tray menu when the settings they use change. Not on
/// every change: re-registering drops the hotkeys for a moment, and a key pressed
/// then (while changing an overlay color, say) would be lost.
fn watch_config(app: AppHandle, core: Arc<App>) {
    let mut rx = core.subscribe_config();
    let relevant = |c: &Config| (c.hotkeys.clone(), c.delay.presets.clone());
    let mut applied = relevant(&core.config());
    tauri::async_runtime::spawn(async move {
        while rx.changed().await.is_ok() {
            let config = rx.borrow_and_update().clone();
            let now = relevant(&config);
            if now == applied {
                continue;
            }
            applied = now;
            hotkeys::register(&app, &config);
            if let Err(e) = tray::rebuild_menu(&app, &core) {
                warn!("could not rebuild the tray menu: {e}");
            }
        }
    });
}

fn updater_configured(app: &AppHandle) -> bool {
    app.config()
        .plugins
        .0
        .get("updater")
        .and_then(|u| u.get("pubkey"))
        .and_then(|k| k.as_str())
        .is_some_and(|k| !k.trim().is_empty())
}

/// Opens (or focuses) the dashboard window, optionally on a specific tab.
pub fn show_dashboard(app: &AppHandle, tab: Option<&str>) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let Some(core) = app.try_state::<Core>() else {
        return;
    };
    let mut url = core.0.urls().dashboard;
    if let Some(tab) = tab {
        url.push('#');
        url.push_str(tab);
    }
    let Ok(url) = url.parse() else { return };
    let result = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
        .title("stream-delay")
        .inner_size(1120.0, 820.0)
        .min_inner_size(420.0, 480.0)
        // Without a handler macOS ignores downloads (the diagnostics file); the
        // default destination is the Downloads folder.
        .on_download(|webview, event| {
            if let DownloadEvent::Finished { path, success, .. } = event {
                let (text, kind) = match (success, path) {
                    (true, Some(p)) => {
                        (format!("Saved to {}", p.display()), MessageDialogKind::Info)
                    }
                    _ => ("The download failed.".to_string(), MessageDialogKind::Error),
                };
                webview
                    .dialog()
                    .message(text)
                    .kind(kind)
                    .title("stream-delay")
                    .show(|_| {});
            }
            true
        })
        .build();
    match result {
        Ok(_) => info!("dashboard opened"),
        Err(e) => error!("could not open the dashboard window: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn the_log_file_starts_again_when_full() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stream-delay.log");
        let mut log = LogFile::create(path.clone(), 100).unwrap();
        for i in 0..30 {
            writeln!(log, "line {i:02}").unwrap();
        }
        log.flush().unwrap();
        let current = std::fs::read_to_string(&path).unwrap();
        let older = std::fs::read_to_string(dir.path().join("stream-delay.1.log")).unwrap();
        assert!(current.len() <= 100 && older.len() <= 100);
        assert!(current.ends_with("line 29\n"), "{current}");
        assert!(older.contains("line 2"), "{older}");
    }
}
