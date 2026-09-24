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
mod bound;
mod changes;
mod dest_key;
pub mod diagnostics;
mod obs_routes;
mod routes;
mod settings;
mod ui;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use axum::Router;
use streamdelay_config::{Config, SecretStore};
use tokio::sync::watch;

pub use app::{App, AppError, AppOptions, MIN_TOKEN_LEN, Overrides, Urls, reachable};
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
    /// The settings in effect: the settings file plus this run's command-line
    /// overrides.
    pub config: RwLock<Config>,
    /// The settings as in the file, without the overrides. Changes are made to
    /// both; only this one is saved.
    pub saved: Mutex<Config>,
    /// Where to save configuration changes (`None`: keep them in memory only).
    pub config_path: Option<PathBuf>,
    /// Held for the whole of a settings change; see [`changes`].
    pub save_lock: Mutex<()>,
    pub secrets: Arc<dyn SecretStore>,
    /// Accepted API tokens, derived from `config.api.token` at startup.
    pub tokens: auth::Tokens,
    /// Requests may name any host (LAN access). Like the listening address, this
    /// changes at the next start, not when the setting is saved.
    pub allow_lan: bool,
    /// Single-use codes for downloading diagnostics; see [`diagnostics`].
    pub download_codes: diagnostics::Codes,
    /// Destination key given on the command line or environment (not persisted).
    pub key_override: Option<String>,
    /// The destination URL `key_override` was given for; see [`AppState::key_override`].
    pub key_override_url: String,
    pub config_tx: watch::Sender<Config>,
    /// Port clients use in the Host header.
    pub port: u16,
    pub restart_required: AtomicBool,
    /// Overlay pages connected (see the events route): how many there are tells
    /// the dock whether the Mask slate can cover the stream.
    pub overlays: watch::Sender<usize>,
    /// Set by the desktop app: checks for an update and offers to install it.
    pub update_check: RwLock<Option<UpdateCheck>>,
    /// The destination (address and key) last given to the relay; see
    /// [`changes`].
    pub applied_destination: Mutex<Option<streamdelay_relay::Destination>>,
    /// Held for the whole of an OBS wizard step (connect, configure, restore),
    /// network calls included, so steps never mix one OBS's secrets with another.
    pub obs_lock: tokio::sync::Mutex<()>,
}

/// See [`App::on_update_check`].
pub(crate) type UpdateCheck = Arc<dyn Fn() + Send + Sync>;

impl AppState {
    pub(crate) fn config(&self) -> Config {
        self.shared.config.read().expect("config lock").clone()
    }

    pub(crate) fn relay(&self) -> &RelayHandle {
        &self.shared.relay
    }

    /// The stored stream key, if it may be sent to `url`: only to the server it
    /// was saved for (see [`dest_key`]).
    pub(crate) fn stored_key(&self, url: &str) -> Option<String> {
        dest_key::for_url(
            self.shared.secrets.as_ref(),
            url,
            &self.saved_destination_url(),
        )
    }

    /// Where the relay should publish with settings `c`.
    pub(crate) fn destination(&self, c: &Config) -> Option<streamdelay_relay::Destination> {
        let url = &c.destination.url;
        let key = self
            .key_override(url)
            .map(str::to_string)
            .or_else(|| self.stored_key(url));
        app::destination(c, key)
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
    routes::router(state(relay, config, secrets, port, None))
}

/// [`router`], saving settings changes to `config_path`.
pub fn router_saving_to(
    relay: RelayHandle,
    config: Config,
    secrets: Arc<dyn SecretStore>,
    port: u16,
    config_path: PathBuf,
) -> Router {
    routes::router(state(relay, config, secrets, port, Some(config_path)))
}

/// State for [`router`] and tests: no command-line key.
pub(crate) fn state(
    relay: RelayHandle,
    config: Config,
    secrets: Arc<dyn SecretStore>,
    port: u16,
    config_path: Option<PathBuf>,
) -> AppState {
    let (config_tx, _) = watch::channel(config.clone());
    let st = AppState {
        shared: Arc::new(Shared {
            relay,
            tokens: auth::Tokens::new(&config.api.token),
            allow_lan: config.api.allow_lan,
            download_codes: Default::default(),
            saved: Mutex::new(config.clone()),
            config: RwLock::new(config),
            config_path,
            save_lock: Mutex::new(()),
            secrets,
            key_override: None,
            key_override_url: String::new(),
            config_tx,
            port,
            restart_required: AtomicBool::new(false),
            overlays: watch::channel(0).0,
            update_check: RwLock::new(None),
            applied_destination: Mutex::new(None),
            obs_lock: tokio::sync::Mutex::new(()),
        }),
    };
    // What a relay started with these settings was given.
    *st.shared
        .applied_destination
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = st.destination(&st.config());
    st
}
