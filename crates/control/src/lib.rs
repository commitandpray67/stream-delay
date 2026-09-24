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
mod dest_key;
pub mod diagnostics;
mod obs_routes;
mod routes;
mod settings;
mod ui;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};

use axum::Router;
use axum::http::StatusCode;
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
    /// Held for the whole of a settings change; see [`AppState::lock_settings`].
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
    /// [`AppState::sync_destination`].
    pub applied_destination: Mutex<Option<streamdelay_relay::Destination>>,
    /// Held for the whole of an OBS wizard step (connect, configure, restore),
    /// network calls included, so steps never mix one OBS's secrets with another.
    pub obs_lock: tokio::sync::Mutex<()>,
}

/// Proof that the settings lock is held; see [`AppState::lock_settings`].
pub(crate) struct SettingsLock<'a> {
    _guard: MutexGuard<'a, ()>,
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

    /// Serializes settings changes. Hold it across everything one change does
    /// (secrets, the settings file, the relay), so changes made at the same time
    /// happen one after the other instead of mixing.
    pub(crate) fn lock_settings(&self) -> SettingsLock<'_> {
        SettingsLock {
            _guard: self
                .shared
                .save_lock
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        }
    }

    /// [`AppState::change_config_locked`], taking the settings lock for it.
    pub(crate) fn change_config<R>(
        &self,
        change: impl FnOnce(&mut Config) -> R,
    ) -> Result<(Config, R), ApiError> {
        let lock = self.lock_settings();
        self.change_config_locked(&lock, change)
    }

    /// Applies `change` to a copy of the settings, saves it (if there is a
    /// settings file) and only then puts it in effect and tells listeners. If
    /// saving fails, nothing changes.
    ///
    /// Only the settings `change` actually changed are saved. The dashboard sends
    /// whole sections, which include this run's command-line overrides; those stay
    /// in effect but never reach the file, unless a change sets another value.
    pub(crate) fn change_config_locked<R>(
        &self,
        _lock: &SettingsLock<'_>,
        change: impl FnOnce(&mut Config) -> R,
    ) -> Result<(Config, R), ApiError> {
        let before = self.config();
        let mut after = before.clone();
        let r = change(&mut after);
        let saved = with_changes(
            &self
                .shared
                .saved
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            &before,
            &after,
        );
        if let Some(path) = &self.shared.config_path {
            saved.save(path).map_err(|e| {
                tracing::warn!("saving settings failed: {e}");
                ApiError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("could not save the settings, so nothing was changed: {e}"),
                )
            })?;
        }
        *self
            .shared
            .saved
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = saved;
        *self.shared.config.write().expect("config lock") = after.clone();
        self.shared.config_tx.send_replace(after.clone());
        Ok((after, r))
    }

    /// The destination in the settings file (not a command-line override).
    pub(crate) fn saved_destination_url(&self) -> String {
        self.shared
            .saved
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .destination
            .url
            .clone()
    }

    /// Gives the relay the destination the settings and saved key now call for,
    /// if it differs from the one it has (another address, or another key for the
    /// same one). Call it after anything that may change either.
    pub(crate) fn sync_destination(&self) -> Result<(), ApiError> {
        let mut applied = self
            .shared
            .applied_destination
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let dest = self.destination(&self.config());
        if *applied != dest {
            tracing::info!(?dest, "destination updated");
            self.relay().set_destination(dest.clone())?;
            *applied = dest;
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use streamdelay_config::MemorySecrets;
    use streamdelay_relay::RelayConfig;

    use super::*;

    #[tokio::test]
    async fn a_change_that_cannot_be_saved_changes_nothing() {
        let relay = streamdelay_relay::start(RelayConfig {
            ingest_bind: "127.0.0.1:0".parse().unwrap(),
            ..Default::default()
        })
        .await
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        // Its folder is a file, so saving fails whoever runs the test.
        std::fs::write(dir.path().join("not-a-folder"), "").unwrap();
        let path = dir.path().join("not-a-folder/config.toml");
        let mut config = Config::default();
        config.api.token = "0123456789abcdef".into();
        let st = state(
            relay,
            config,
            Arc::new(MemorySecrets::default()),
            7788,
            Some(path),
        );
        let notified = st.shared.config_tx.subscribe();
        let before = st.config();
        let r = st.change_config(|c| c.destination.url = "rtmp://ingest.example.net/live".into());
        assert!(r.is_err());
        assert_eq!(st.config(), before);
        assert_eq!(st.saved_destination_url(), before.destination.url);
        assert!(
            !notified.has_changed().unwrap(),
            "listeners told of a change that did not happen"
        );
    }

    #[test]
    fn only_what_a_change_changed_is_saved() {
        let saved = Config::default();
        // In effect: a command-line maximum.
        let mut before = saved.clone();
        before.delay.max_seconds = 60;
        let mut after = before.clone();
        after.delay.presets[1].seconds = 7.0;
        after.obs.backup = Some(streamdelay_config::ObsBackup {
            service_type: "rtmp_common".into(),
            obs: Some("127.0.0.1:4455".into()),
            settings_json: None,
        });
        let s = with_changes(&saved, &before, &after);
        assert_eq!(s.delay.max_seconds, 120, "the override was saved");
        assert_eq!(s.delay.presets[1].seconds, 7.0);
        assert_eq!(s.obs.backup, after.obs.backup);
        // Settings a change removes are removed.
        let s = with_changes(&s, &after, &before);
        assert_eq!(s.obs.backup, None);
        assert_eq!(s.delay.presets, saved.delay.presets);
        assert_eq!(s, saved);
    }

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
