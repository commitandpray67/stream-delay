//! The destination stream key. It is kept in the secret store together with the
//! server it was saved for (see [`bound`]), and only ever used for that server:
//! even if removing it fails when the destination changes, it cannot reach the
//! new one.

use streamdelay_config::{SecretError, SecretStore, secret};

use crate::app::{different_server, split_url_key};
use crate::bound::{self, Saved};

fn read(secrets: &dyn SecretStore) -> Option<Saved<String>> {
    bound::load(secrets, secret::DESTINATION_KEY, Some)
}

/// Saves `key` for the server of the destination URL `server`.
pub(crate) fn save(secrets: &dyn SecretStore, server: &str, key: &str) -> Result<(), SecretError> {
    bound::save(
        secrets,
        secret::DESTINATION_KEY,
        &split_url_key(server).0,
        &key,
    )
}

/// The saved key, if it belongs to `url`'s server. A plain key saved by an older
/// version belongs to `legacy_server` (the destination in the settings file).
pub(crate) fn for_url(secrets: &dyn SecretStore, url: &str, legacy_server: &str) -> Option<String> {
    let saved = read(secrets)?;
    let server = saved.owner.as_deref().unwrap_or(legacy_server);
    (!saved.value.is_empty() && !different_server(server, url)).then_some(saved.value)
}

/// The saved key whatever its server, for redaction.
pub(crate) fn any(secrets: &dyn SecretStore) -> Option<String> {
    read(secrets).map(|s| s.value).filter(|k| !k.is_empty())
}

/// Binds a plain key saved by an older version to `server`, the destination it
/// was used for. Returns true if there was one.
pub(crate) fn bind_legacy(secrets: &dyn SecretStore, server: &str) -> Result<bool, SecretError> {
    match read(secrets) {
        Some(Saved { owner: None, value }) if !value.is_empty() => {
            save(secrets, server, &value).map(|()| true)
        }
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use streamdelay_config::MemorySecrets;

    use super::*;

    const TWITCH: &str = "rtmp://live.twitch.tv/app";
    const OTHER: &str = "rtmp://ingest.example.net/live";

    #[test]
    fn a_key_is_only_used_for_its_server() {
        let s = MemorySecrets::default();
        save(&s, TWITCH, "live_1_a").unwrap();
        assert_eq!(for_url(&s, TWITCH, OTHER).as_deref(), Some("live_1_a"));
        assert_eq!(
            for_url(
                &s,
                "rtmps://ingest.global-contribute.live-video.net/app",
                OTHER
            )
            .as_deref(),
            Some("live_1_a"),
            "another Twitch server"
        );
        // Whatever the settings file says: a leftover key never goes elsewhere.
        assert_eq!(for_url(&s, OTHER, OTHER), None);
        assert_eq!(any(&s).as_deref(), Some("live_1_a"));
    }

    #[test]
    fn plain_keys_from_older_versions_belong_to_the_saved_destination() {
        let s = MemorySecrets::default();
        s.set(secret::DESTINATION_KEY, "live_1_a").unwrap();
        assert_eq!(for_url(&s, TWITCH, TWITCH).as_deref(), Some("live_1_a"));
        assert_eq!(for_url(&s, OTHER, TWITCH), None);
        assert!(bind_legacy(&s, TWITCH).unwrap());
        assert!(!bind_legacy(&s, OTHER).unwrap(), "already bound");
        assert_eq!(for_url(&s, OTHER, OTHER), None);
        assert_eq!(for_url(&s, TWITCH, OTHER).as_deref(), Some("live_1_a"));
    }

    #[test]
    fn the_server_is_saved_without_a_key_in_it() {
        let s = MemorySecrets::default();
        save(&s, "rtmp://live.twitch.tv/app/live_9_url", "live_1_a").unwrap();
        let raw = s.get(secret::DESTINATION_KEY).unwrap();
        assert!(!raw.contains("live_9_url"), "{raw}");
    }
}
