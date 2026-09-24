//! The destination stream key. It is kept in the secret store together with the
//! server it was saved for, and only ever used for that server: even if removing
//! it fails when the destination changes, it cannot reach the new one.

use serde::{Deserialize, Serialize};
use streamdelay_config::{SecretStore, secret};

use crate::app::{different_server, split_url_key};

#[derive(Serialize, Deserialize)]
struct Bound {
    server: String,
    key: String,
}

/// What the store holds: a key and the server it belongs to. `None` for the
/// server means a plain key saved by an older version.
fn read(secrets: &dyn SecretStore) -> Option<(Option<String>, String)> {
    let raw = secrets.get(secret::DESTINATION_KEY)?;
    Some(match serde_json::from_str::<Bound>(&raw) {
        Ok(b) => (Some(b.server), b.key),
        Err(_) => (None, raw),
    })
}

/// Saves `key` for the server of the destination URL `server`.
pub(crate) fn save(secrets: &dyn SecretStore, server: &str, key: &str) -> Result<(), String> {
    let bound = Bound {
        server: split_url_key(server).0,
        key: key.to_string(),
    };
    let text = serde_json::to_string(&bound).map_err(|e| e.to_string())?;
    secrets.set(secret::DESTINATION_KEY, &text)
}

/// The saved key, if it belongs to `url`'s server. A plain key saved by an older
/// version belongs to `legacy_server` (the destination in the settings file).
pub(crate) fn for_url(secrets: &dyn SecretStore, url: &str, legacy_server: &str) -> Option<String> {
    let (server, key) = read(secrets)?;
    let server = server.as_deref().unwrap_or(legacy_server);
    (!key.is_empty() && !different_server(server, url)).then_some(key)
}

/// The saved key whatever its server, for redaction.
pub(crate) fn any(secrets: &dyn SecretStore) -> Option<String> {
    read(secrets).map(|(_, key)| key).filter(|k| !k.is_empty())
}

/// Binds a plain key saved by an older version to `server`, the destination it
/// was used for. Returns true if there was one.
pub(crate) fn bind_legacy(secrets: &dyn SecretStore, server: &str) -> Result<bool, String> {
    match read(secrets) {
        Some((None, key)) if !key.is_empty() => save(secrets, server, &key).map(|()| true),
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
