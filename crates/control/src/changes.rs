//! Settings changes. Every change to the settings, the saved stream key and the
//! destination the relay was given goes through here, one at a time: a change is
//! staged, saved, and only then put in effect. If it cannot be saved, nothing
//! changes, the stream key included.

use std::sync::{MutexGuard, PoisonError};

use axum::http::StatusCode;
use streamdelay_config::{Config, ConfigError, SecretError, secret};
use streamdelay_relay::RelayError;
use thiserror::Error;

use crate::routes::ApiError;
use crate::{AppState, dest_key};

/// Proof that the settings lock is held; see [`AppState::lock_settings`].
pub(crate) struct SettingsLock<'a> {
    _guard: MutexGuard<'a, ()>,
}

#[derive(Debug, Error)]
pub(crate) enum ChangeError {
    #[error("{0}")]
    Invalid(String),
    #[error("could not save the settings, so nothing was changed: {0}")]
    Save(ConfigError),
    #[error("could not update the saved stream key, so nothing was changed: {0}")]
    Key(SecretError),
    #[error(
        "could not save the settings, so nothing was changed: {save}. The saved stream key \
         had already been changed and could not be put back ({restore}): enter it again on \
         the Setup tab."
    )]
    KeyNotRestored {
        save: Box<ConfigError>,
        restore: SecretError,
    },
    /// Saved and in effect, but the relay could not be given the destination.
    #[error(transparent)]
    Relay(#[from] RelayError),
}

impl From<ChangeError> for ApiError {
    fn from(e: ChangeError) -> Self {
        match e {
            ChangeError::Invalid(why) => ApiError::bad_request(why),
            ChangeError::Relay(e) => e.into(),
            e => ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    }
}

/// What a change does to the saved stream key.
pub(crate) enum KeyChange {
    Keep,
    /// Saves `key` for the server of the destination URL `server`.
    Save {
        server: String,
        key: String,
    },
    /// Forgets the saved key.
    Forget,
}

impl AppState {
    /// Serializes settings changes. Hold it across everything one change does
    /// (reading the settings it depends on included), so changes made at the same
    /// time happen one after the other instead of mixing.
    pub(crate) fn lock_settings(&self) -> SettingsLock<'_> {
        SettingsLock {
            _guard: self
                .shared
                .save_lock
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        }
    }

    /// [`AppState::change_settings_locked`], taking the settings lock for it.
    pub(crate) fn change_settings<R>(
        &self,
        key: KeyChange,
        change: impl FnOnce(&mut Config) -> R,
    ) -> Result<(Config, R), ChangeError> {
        let lock = self.lock_settings();
        self.change_settings_locked(&lock, key, change)
    }

    /// Changes the saved stream key as `key` says and the settings with `change`,
    /// saves both, then gives the relay the destination they call for. If the
    /// settings cannot be saved, the saved key is put back as it was.
    pub(crate) fn change_settings_locked<R>(
        &self,
        lock: &SettingsLock<'_>,
        key: KeyChange,
        change: impl FnOnce(&mut Config) -> R,
    ) -> Result<(Config, R), ChangeError> {
        let secrets = self.shared.secrets.as_ref();
        // The saved key as it was, to put back if the settings cannot be saved.
        let key_before = match key {
            KeyChange::Keep => None,
            _ => Some(
                secrets
                    .try_get(secret::DESTINATION_KEY)
                    .map_err(ChangeError::Key)?,
            ),
        };
        match &key {
            KeyChange::Keep => {}
            KeyChange::Save { server, key } => {
                dest_key::save(secrets, server, key).map_err(ChangeError::Key)?
            }
            KeyChange::Forget => secrets
                .delete(secret::DESTINATION_KEY)
                .map_err(ChangeError::Key)?,
        }
        let changed = match self.save_settings(lock, change) {
            Ok(changed) => changed,
            Err(save) => {
                let restored = match key_before {
                    None => Ok(()),
                    Some(Some(raw)) => secrets.set(secret::DESTINATION_KEY, &raw),
                    Some(None) => secrets.delete(secret::DESTINATION_KEY),
                };
                return Err(match restored {
                    Ok(()) => ChangeError::Save(save),
                    Err(restore) => {
                        tracing::error!("could not put the saved stream key back: {restore}");
                        ChangeError::KeyNotRestored {
                            save: Box::new(save),
                            restore,
                        }
                    }
                });
            }
        };
        self.sync_destination(lock)?;
        Ok(changed)
    }

    /// Saves `key` as the stream key for the destination in the settings file
    /// (the one the key field is shown with).
    pub(crate) fn save_destination_key(&self, key: &str) -> Result<(), ChangeError> {
        let key = key.trim();
        if key.is_empty() || key.len() > 512 || key.chars().any(char::is_control) {
            return Err(ChangeError::Invalid(
                "that does not look like a stream key".into(),
            ));
        }
        let lock = self.lock_settings();
        dest_key::save(
            self.shared.secrets.as_ref(),
            &self.saved_destination_url(),
            key,
        )
        .map_err(ChangeError::Key)?;
        self.sync_destination(&lock)?;
        self.shared.config_tx.send_modify(|_| {});
        Ok(())
    }

    /// Forgets the saved stream key.
    pub(crate) fn forget_destination_key(&self) -> Result<(), ChangeError> {
        let lock = self.lock_settings();
        self.shared
            .secrets
            .delete(secret::DESTINATION_KEY)
            .map_err(ChangeError::Key)?;
        self.sync_destination(&lock)?;
        self.shared.config_tx.send_modify(|_| {});
        Ok(())
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

    /// Applies `change` to a copy of the settings, saves it (if there is a
    /// settings file) and only then puts it in effect and tells listeners. If
    /// saving fails, nothing changes.
    ///
    /// Only the settings `change` actually changed are saved. The dashboard sends
    /// whole sections, which include this run's command-line overrides; those stay
    /// in effect but never reach the file, unless a change sets another value.
    fn save_settings<R>(
        &self,
        _lock: &SettingsLock<'_>,
        change: impl FnOnce(&mut Config) -> R,
    ) -> Result<(Config, R), ConfigError> {
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
            saved.save(path).inspect_err(|e| {
                tracing::warn!("saving settings failed: {e}");
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

    /// Gives the relay the destination the settings and saved key now call for,
    /// if it differs from the one it has (another address, or another key for the
    /// same one).
    fn sync_destination(&self, _lock: &SettingsLock<'_>) -> Result<(), RelayError> {
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use streamdelay_config::{MemorySecrets, SecretStore};
    use streamdelay_relay::{DestinationKey, RelayConfig};

    use super::*;

    const TWITCH: &str = "rtmp://live.twitch.tv/app";

    async fn setup(path: Option<PathBuf>) -> (AppState, Arc<MemorySecrets>) {
        let relay = streamdelay_relay::start(RelayConfig {
            ingest_bind: "127.0.0.1:0".parse().unwrap(),
            ..Default::default()
        })
        .await
        .unwrap();
        let mut config = Config::default();
        config.api.token = "0123456789abcdef".into();
        let secrets = Arc::new(MemorySecrets::default());
        let st = crate::state(relay, config, secrets.clone(), 7788, path);
        (st, secrets)
    }

    fn applied_key(st: &AppState) -> Option<String> {
        match st.shared.applied_destination.lock().unwrap().clone()?.key {
            DestinationKey::Fixed(k) => Some(k),
            DestinationKey::Passthrough => None,
        }
    }

    fn set_url(url: &str) -> impl FnOnce(&mut Config) + '_ {
        move |c| c.destination.url = url.into()
    }

    #[tokio::test]
    async fn a_change_that_cannot_be_saved_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        // Its folder is a file, so saving fails whoever runs the test.
        std::fs::write(dir.path().join("not-a-folder"), "").unwrap();
        let (st, _) = setup(Some(dir.path().join("not-a-folder/config.toml"))).await;
        let notified = st.shared.config_tx.subscribe();
        let before = st.config();
        let r = st.change_settings(KeyChange::Keep, set_url("rtmp://ingest.example.net/live"));
        assert!(matches!(r, Err(ChangeError::Save(_))));
        assert_eq!(st.config(), before);
        assert_eq!(st.saved_destination_url(), before.destination.url);
        assert!(
            !notified.has_changed().unwrap(),
            "listeners told of a change that did not happen"
        );
    }

    #[tokio::test]
    async fn a_change_that_cannot_be_saved_keeps_the_key_too() {
        let dir = tempfile::tempdir().unwrap();
        let (st, secrets) = setup(Some(dir.path().join("config.toml"))).await;
        st.save_destination_key("live_1_a").unwrap();
        let saved = secrets.get(secret::DESTINATION_KEY).unwrap();
        // From now on the settings file cannot be written.
        std::fs::create_dir(dir.path().join("config.toml.tmp")).unwrap();
        for key in [
            // Another server: the key would be forgotten.
            KeyChange::Forget,
            // A new key: it would replace the saved one.
            KeyChange::Save {
                server: TWITCH.into(),
                key: "live_1_b".into(),
            },
        ] {
            let r = st.change_settings(key, set_url("rtmp://ingest.example.net/live"));
            let e = r.unwrap_err();
            assert!(matches!(e, ChangeError::Save(_)), "{e}");
            assert!(e.to_string().contains("nothing was changed"), "{e}");
            assert_eq!(
                secrets.get(secret::DESTINATION_KEY).as_deref(),
                Some(saved.as_str())
            );
            assert_eq!(st.config().destination.url, TWITCH);
            assert_eq!(applied_key(&st).as_deref(), Some("live_1_a"));
        }
    }

    #[tokio::test]
    async fn every_change_reaches_the_relay() {
        let (st, _) = setup(None).await;
        st.change_settings(
            KeyChange::Save {
                server: TWITCH.into(),
                key: "live_1_a".into(),
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(applied_key(&st).as_deref(), Some("live_1_a"));
        st.save_destination_key("live_1_b").unwrap();
        assert_eq!(applied_key(&st).as_deref(), Some("live_1_b"));
        // Another server: the key is not sent there.
        st.change_settings(KeyChange::Keep, set_url("rtmp://ingest.example.net/live"))
            .unwrap();
        assert_eq!(applied_key(&st).as_deref(), Some(""));
        st.change_settings(KeyChange::Keep, set_url(TWITCH))
            .unwrap();
        assert_eq!(applied_key(&st).as_deref(), Some("live_1_b"));
        st.forget_destination_key().unwrap();
        assert_eq!(applied_key(&st).as_deref(), Some(""));
        let e = st.save_destination_key(" \n ").unwrap_err();
        assert!(matches!(e, ChangeError::Invalid(_)), "{e}");
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let (st, _) = setup(Some(path.clone())).await;
        let threads: Vec<_> = (0..16)
            .map(|i| {
                let st = st.clone();
                std::thread::spawn(move || {
                    st.change_settings(KeyChange::Keep, |c| {
                        c.hotkeys.presets.push(format!("key {i}"))
                    })
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
