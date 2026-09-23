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
use streamdelay_config::{Config, SecretStore, secret};
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
    ///
    /// Only the settings `change` actually changed are saved. The dashboard sends
    /// whole sections, which include this run's command-line overrides; those stay
    /// in effect but never reach the file, unless a change sets another value.
    pub(crate) fn change_config<R>(
        &self,
        change: impl FnOnce(&mut Config) -> R,
    ) -> Result<(Config, R), ApiError> {
        let _saving = self
            .shared
            .save_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (before, config, r) = {
            let mut c = self.shared.config.write().expect("config lock");
            let before = c.clone();
            let r = change(&mut c);
            (before, c.clone(), r)
        };
        let saved = {
            let mut s = self
                .shared
                .saved
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            *s = with_changes(&s, &before, &config);
            s.clone()
        };
        if let Some(path) = &self.shared.config_path {
            saved.save(path).map_err(|e| {
                tracing::warn!("saving settings failed: {e}");
                ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
            })?;
        }
        Ok((config, r))
    }

    /// The stored stream key, if it may be sent to `url`. It belongs to the
    /// destination in the settings file: a destination given on the command line
    /// for another server must not receive it.
    pub(crate) fn stored_key(&self, url: &str) -> Option<String> {
        let saved_url = self
            .shared
            .saved
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .destination
            .url
            .clone();
        if app::different_server(&saved_url, url) {
            return None;
        }
        self.shared
            .secrets
            .get(secret::DESTINATION_KEY)
            .filter(|k| !k.is_empty())
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

/// `saved` with every setting that differs between `before` and `after` set as in
/// `after`.
fn with_changes(saved: &Config, before: &Config, after: &Config) -> Config {
    let json = |c: &Config| serde_json::to_value(c);
    let merged = match (json(saved), json(before), json(after)) {
        (Ok(mut s), Ok(b), Ok(a)) => {
            copy_changes(&mut s, &b, &a);
            serde_json::from_value(s).map_err(|e| e.to_string())
        }
        (Err(e), ..) | (_, Err(e), _) | (.., Err(e)) => Err(e.to_string()),
    };
    merged.unwrap_or_else(|e| {
        // Not expected; saving the whole change is better than losing it.
        tracing::warn!("could not work out which settings changed: {e}");
        after.clone()
    })
}

fn copy_changes(
    target: &mut serde_json::Value,
    before: &serde_json::Value,
    after: &serde_json::Value,
) {
    use serde_json::Value;
    if before == after {
        return;
    }
    match (target, before, after) {
        (Value::Object(t), Value::Object(b), Value::Object(a)) => {
            for (k, av) in a {
                match (b.get(k), t.get_mut(k)) {
                    (Some(bv), Some(tv)) => copy_changes(tv, bv, av),
                    (Some(bv), None) if bv == av => {}
                    _ => {
                        t.insert(k.clone(), av.clone());
                    }
                }
            }
            for k in b.keys().filter(|k| !a.contains_key(*k)) {
                t.remove(k);
            }
        }
        (t, _, a) => *t = a.clone(),
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
