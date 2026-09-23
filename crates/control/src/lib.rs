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

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use axum::Router;
use axum::http::StatusCode;
use streamdelay_config::{Config, SecretStore};
use tokio::sync::watch;

pub use app::{App, AppError, AppOptions, Overrides, Urls, reachable};
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
    pub config_path: Option<PathBuf>,
    /// Held while a change is made and saved; see [`AppState::change_config`].
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
}

impl AppState {
    pub(crate) fn config(&self) -> Config {
        self.shared.config.read().expect("config lock").clone()
    }

    pub(crate) fn relay(&self) -> &RelayHandle {
        &self.shared.relay
    }

    /// Applies `change` to the settings and saves them, if there is a settings file.
    /// Changes are made and saved one at a time, so a slower save can never
    /// overwrite a newer change in the file.
    pub(crate) fn change_config<R>(
        &self,
        change: impl FnOnce(&mut Config) -> R,
    ) -> Result<(Config, R), ApiError> {
        let _saving = self
            .shared
            .save_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (config, r) = {
            let mut c = self.shared.config.write().expect("config lock");
            let r = change(&mut c);
            (c.clone(), r)
        };
        if let Some(path) = &self.shared.config_path {
            config.save(path).map_err(|e| {
                tracing::warn!("saving settings failed: {e}");
                ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
            })?;
        }
        Ok((config, r))
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

/// State for [`router`] and tests: no command-line key.
pub(crate) fn state(
    relay: RelayHandle,
    config: Config,
    secrets: Arc<dyn SecretStore>,
    port: u16,
    config_path: Option<PathBuf>,
) -> AppState {
    let (config_tx, _) = watch::channel(config.clone());
    AppState {
        shared: Arc::new(Shared {
            relay,
            tokens: auth::Tokens::new(&config.api.token),
            allow_lan: config.api.allow_lan,
            download_codes: Default::default(),
            config: RwLock::new(config),
            config_path,
            save_lock: Mutex::new(()),
            secrets,
            key_override: None,
            key_override_url: String::new(),
            config_tx,
            port,
            restart_required: AtomicBool::new(false),
        }),
    }
}

#[cfg(test)]
mod tests {
    use streamdelay_config::MemorySecrets;
    use streamdelay_relay::RelayConfig;

    use super::*;

    #[tokio::test]
    async fn concurrent_changes_all_reach_the_settings_file() {
        let relay = streamdelay_relay::start(RelayConfig {
            ingest_bind: "127.0.0.1:0".parse().unwrap(),
            ..Default::default()
        })
        .await
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config::default();
        config.api.token = "0123456789abcdef".into();
        let st = state(
            relay,
            config,
            Arc::new(MemorySecrets::default()),
            7788,
            Some(path.clone()),
        );
        let threads: Vec<_> = (0..16)
            .map(|i| {
                let st = st.clone();
                std::thread::spawn(move || {
                    st.change_config(|c| c.hotkeys.presets.push(format!("key {i}")))
                        .unwrap()
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let saved = Config::load_or_create(&path).unwrap();
        assert_eq!(
            saved.hotkeys.presets.len(),
            5 + 16,
            "changes lost in the file"
        );
        assert_eq!(saved, st.config());
    }
}
