//! Local control API, settings and embedded web UI for stream-delay.
//!
//! Security model (see `SECURITY.md`): the server binds to localhost by default and
//! every `/api` request must carry the per-install token. The `Host` header must name
//! this server (blocking DNS rebinding) and cross-origin browser requests are refused,
//! so a web page the streamer happens to visit cannot drive the delay.

mod app;
mod auth;
mod obs_routes;
mod routes;
mod settings;
mod ui;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};

use axum::Router;
use streamdelay_config::{Config, SecretStore};
use tokio::sync::watch;

pub use app::{App, AppError, AppOptions, Overrides, Urls};
pub use routes::ApiError;
pub use streamdelay_config::Preset;
use streamdelay_relay::RelayHandle;

/// Shared state for request handlers.
#[derive(Clone)]
pub struct AppState {
    pub(crate) shared: Arc<Shared>,
}

pub(crate) struct Shared {
    pub relay: RelayHandle,
    pub config: RwLock<Config>,
    /// Where to save configuration changes (`None`: keep them in memory only).
    pub config_path: Option<std::path::PathBuf>,
    pub secrets: Arc<dyn SecretStore>,
    /// Destination key given on the command line or environment (not persisted).
    pub key_override: Option<String>,
    pub config_tx: watch::Sender<Config>,
    /// Port clients use in the Host header.
    pub port: u16,
    pub restart_required: AtomicBool,
}

impl AppState {
    pub(crate) fn config(&self) -> Config {
        self.shared.config.read().expect("config lock").clone()
    }

    pub(crate) fn relay(&self) -> &RelayHandle {
        &self.shared.relay
    }
}

/// Builds the router around an existing relay (used by tests and embedders).
pub fn router(
    relay: RelayHandle,
    config: Config,
    secrets: Arc<dyn SecretStore>,
    port: u16,
) -> Router {
    let (config_tx, _) = watch::channel(config.clone());
    let state = AppState {
        shared: Arc::new(Shared {
            relay,
            config: RwLock::new(config),
            config_path: None,
            secrets,
            key_override: None,
            config_tx,
            port,
            restart_required: AtomicBool::new(false),
        }),
    };
    routes::router(state)
}
