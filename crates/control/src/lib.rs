//! Local control API, settings and embedded web UI for stream-delay.
//!
//! Security model (see `SECURITY.md`): the server binds to localhost by default and
//! every `/api` request must carry a token. The dashboard's token can do everything;
//! dock links carry a token that can only control the delay, and overlay links one
//! that can only read the state. The `Host` header must name this server (blocking
//! DNS rebinding) and cross-origin browser requests are refused, so a web page the
//! streamer happens to visit cannot drive the delay.

mod app;
mod auth;
pub mod diagnostics;
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
pub use auth::{Scope, scoped_token};
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
    /// Accepted API tokens, derived from `config.api.token` at startup.
    pub tokens: auth::Tokens,
    /// Destination key given on the command line or environment (not persisted).
    pub key_override: Option<String>,
    /// The destination URL `key_override` was given for; see [`AppState::key_override`].
    pub key_override_url: String,
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

    /// The command-line destination key, unless the destination has since been
    /// changed to another server: it must only ever go where it was meant to.
    pub(crate) fn key_override(&self, url: &str) -> Option<&str> {
        self.shared
            .key_override
            .as_deref()
            .filter(|_| !app::different_server(&self.shared.key_override_url, url))
    }
}

/// Builds the router around an existing relay (used by tests and embedders).
pub fn router(
    relay: RelayHandle,
    config: Config,
    secrets: Arc<dyn SecretStore>,
    port: u16,
) -> Router {
    routes::router(state(relay, config, secrets, port))
}

/// State for [`router`]: settings kept in memory, no command-line key.
pub(crate) fn state(
    relay: RelayHandle,
    config: Config,
    secrets: Arc<dyn SecretStore>,
    port: u16,
) -> AppState {
    let (config_tx, _) = watch::channel(config.clone());
    AppState {
        shared: Arc::new(Shared {
            relay,
            tokens: auth::Tokens::new(&config.api.token),
            config: RwLock::new(config),
            config_path: None,
            secrets,
            key_override: None,
            key_override_url: String::new(),
            config_tx,
            port,
            restart_required: AtomicBool::new(false),
        }),
    }
}
