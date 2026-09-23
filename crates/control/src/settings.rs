//! Settings endpoints: read and change configuration, and store the stream key.

use std::sync::atomic::Ordering;

use axum::extract::State;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use streamdelay_config::{
    Config, DelayConfig, DestinationConfig, HotkeyConfig, OverlayConfig, SERVICES, secret,
};
use streamdelay_relay::RtmpUrl;
use tracing::info;

use crate::AppState;
use crate::app::{Urls, different_server, split_url_key, urls};
use crate::auth::Scope;
use crate::routes::ApiError;

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/config", get(get_config).put(update_config))
        .route("/api/v1/destination/key", put(set_key).delete(delete_key))
}

#[derive(Debug, Serialize)]
pub(crate) struct ServiceInfo {
    id: &'static str,
    name: &'static str,
    url: &'static str,
}

/// Configuration as shown to the dashboard: no token, no secrets.
#[derive(Debug, Serialize)]
pub(crate) struct PublicConfig {
    /// Always `admin`; lets clients tell this apart from [`LimitedConfig`].
    scope: Scope,
    config: Config,
    destination_key_set: bool,
    secrets_backend: String,
    urls: Urls,
    services: Vec<ServiceInfo>,
    restart_required: bool,
    version: &'static str,
    /// See [`crate::ui::ui_build`].
    ui_build: Option<&'static str>,
}

pub(crate) fn public_config(st: &AppState) -> PublicConfig {
    let mut config = st.config();
    config.api.token = String::new();
    // Shown as `urls.obs_key` instead, which diagnostics leave out.
    config.ingest.key = None;
    // Keys are moved out of the URL when it is saved; never show one regardless.
    config.destination.url = split_url_key(&config.destination.url).0;
    // Only there until moved to the secret store; may hold a server password.
    if let Some(backup) = &mut config.obs.backup {
        backup.settings_json = None;
    }
    let key_set = st.key_override(&config.destination.url).is_some()
        || st.stored_key(&config.destination.url).is_some();
    PublicConfig {
        scope: Scope::Admin,
        config,
        destination_key_set: key_set,
        secrets_backend: st.shared.secrets.describe(),
        urls: urls(st),
        services: SERVICES
            .iter()
            .map(|s| ServiceInfo {
                id: s.id,
                name: s.name,
                url: s.url,
            })
            .collect(),
        restart_required: st.shared.restart_required.load(Ordering::Relaxed),
        version: env!("CARGO_PKG_VERSION"),
        ui_build: crate::ui::ui_build(),
    }
}

/// What dock and overlay links see: the settings they display, nothing else.
#[derive(Debug, Serialize)]
pub(crate) struct LimitedConfig {
    scope: Scope,
    config: LimitedSettings,
    urls: LimitedUrls,
    version: &'static str,
    ui_build: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct LimitedSettings {
    delay: DelayConfig,
    overlay: OverlayConfig,
}

#[derive(Debug, Serialize)]
struct LimitedUrls {
    obs_server: String,
}

pub(crate) fn limited_config(st: &AppState, scope: Scope) -> LimitedConfig {
    let c = st.config();
    LimitedConfig {
        scope,
        config: LimitedSettings {
            delay: c.delay,
            overlay: c.overlay,
        },
        urls: LimitedUrls {
            obs_server: urls(st).obs_server,
        },
        version: env!("CARGO_PKG_VERSION"),
        ui_build: crate::ui::ui_build(),
    }
}

async fn get_config(State(st): State<AppState>) -> Json<PublicConfig> {
    Json(public_config(&st))
}

/// Partial update: every section is optional.
#[derive(Debug, Deserialize)]
struct SettingsUpdate {
    destination: Option<DestinationConfig>,
    delay: Option<DelayConfig>,
    overlay: Option<OverlayConfig>,
    hotkeys: Option<HotkeyConfig>,
    grace_seconds: Option<u64>,
    allow_lan: Option<bool>,
}

fn valid_color(c: &str) -> bool {
    let hex = c.strip_prefix('#').unwrap_or("");
    matches!(hex.len(), 3 | 6 | 8) && hex.chars().all(|ch| ch.is_ascii_hexdigit())
}

/// Longest encoder grace period, in seconds.
const MAX_GRACE_SECONDS: u64 = 600;

/// Limits that keep the relay working, for settings from any source: the API, the
/// config file and the command line.
pub(crate) fn validate_limits(d: &DelayConfig, grace_seconds: u64) -> Result<(), String> {
    if !(5..=900).contains(&d.max_seconds) {
        return Err("maximum delay must be between 5 and 900 seconds".into());
    }
    if !(0.0..=d.max_seconds as f64).contains(&d.start_seconds) {
        return Err("start delay must be between 0 and the maximum delay".into());
    }
    if !(16..=16_384).contains(&d.ram_cap_mb) {
        return Err("memory cap must be between 16 and 16384 MiB".into());
    }
    if grace_seconds > MAX_GRACE_SECONDS {
        return Err(format!(
            "the grace period must be at most {MAX_GRACE_SECONDS} seconds"
        ));
    }
    Ok(())
}

fn validate(u: &SettingsUpdate, current: &Config) -> Result<(), String> {
    if let Some(d) = &u.destination
        && !d.url.trim().is_empty()
    {
        RtmpUrl::parse(&d.url).map_err(|e| format!("destination URL: {e}"))?;
    }
    let grace = u.grace_seconds.unwrap_or(current.ingest.grace_seconds);
    validate_limits(u.delay.as_ref().unwrap_or(&current.delay), grace)?;
    if let Some(d) = &u.delay {
        if d.presets.is_empty() || d.presets.len() > 10 {
            return Err("configure between 1 and 10 presets".into());
        }
        let max = d.max_seconds as f64;
        for p in &d.presets {
            if !p.seconds.is_finite() || p.seconds < 0.0 || p.seconds > max {
                return Err(format!("preset {} s is outside 0..{max} s", p.seconds));
            }
        }
    }
    if let Some(o) = &u.overlay {
        for c in [&o.accent_color, &o.background_color, &o.text_color] {
            if !valid_color(c) {
                return Err(format!("'{c}' is not a hex color like #9147ff"));
            }
        }
        if o.mask_title.len() > 200 || o.mask_subtitle.len() > 400 || o.mask_image.len() > 2048 {
            return Err("overlay text is too long".into());
        }
        if !["top-left", "top-right", "bottom-left", "bottom-right"]
            .contains(&o.badge_position.as_str())
        {
            return Err(
                "badge position must be top-left, top-right, bottom-left or bottom-right".into(),
            );
        }
    }
    Ok(())
}

async fn update_config(
    State(st): State<AppState>,
    Json(mut update): Json<SettingsUpdate>,
) -> Result<Json<PublicConfig>, ApiError> {
    validate(&update, &st.config()).map_err(ApiError::bad_request)?;
    // A key typed into the URL (rtmp://host/app/<key>) is stored like one entered
    // in the key field, so the URL shown to the UI and saved in config.toml never
    // contains it.
    let mut url_key = None;
    if let Some(d) = &mut update.destination {
        let (url, key) = split_url_key(&d.url);
        d.url = url;
        url_key = key;
    }
    // The stored key belongs to the destination in the settings file (the one in
    // effect may be a command-line override).
    let old_url = st
        .shared
        .saved
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .destination
        .url
        .clone();
    let secret_err = |e| ApiError(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e);
    if let Some(key) = &url_key {
        st.shared
            .secrets
            .set(secret::DESTINATION_KEY, key)
            .map_err(secret_err)?;
    } else if let Some(d) = &update.destination
        && different_server(&old_url, &d.url)
    {
        // The stored key belongs to the old server: never send it to another one.
        st.shared
            .secrets
            .delete(secret::DESTINATION_KEY)
            .map_err(secret_err)?;
        info!("destination server changed; the stored stream key was removed");
    }
    let (new_config, (destination_changed, keep_buffer_changed, restart)) =
        st.change_config(|c| {
            let mut restart = false;
            let mut destination_changed = false;
            let mut keep_buffer_changed = false;
            if let Some(d) = &update.destination {
                destination_changed = *d != c.destination;
                c.destination = d.clone();
            }
            if let Some(d) = &update.delay {
                restart |=
                    d.max_seconds != c.delay.max_seconds || d.ram_cap_mb != c.delay.ram_cap_mb;
                keep_buffer_changed = d.keep_buffer != c.delay.keep_buffer;
                c.delay = d.clone();
            }
            if let Some(o) = &update.overlay {
                c.overlay = o.clone();
            }
            if let Some(h) = &update.hotkeys {
                c.hotkeys = h.clone();
            }
            if let Some(g) = update.grace_seconds {
                restart |= g != c.ingest.grace_seconds;
                c.ingest.grace_seconds = g;
            }
            if let Some(l) = update.allow_lan {
                restart |= l != c.api.allow_lan;
                c.api.allow_lan = l;
            }
            (destination_changed, keep_buffer_changed, restart)
        })?;
    if restart {
        st.shared.restart_required.store(true, Ordering::Relaxed);
    }
    if destination_changed {
        apply_destination(&st, &new_config)?;
    }
    if keep_buffer_changed {
        st.relay().set_keep_history(new_config.delay.keep_buffer)?;
    }
    st.shared.config_tx.send_replace(new_config);
    Ok(Json(public_config(&st)))
}

fn apply_destination(st: &AppState, c: &Config) -> Result<(), ApiError> {
    let dest = st.destination(c);
    info!(?dest, "destination updated");
    st.relay().set_destination(dest)?;
    Ok(())
}

#[derive(Debug, Deserialize)]
struct KeyBody {
    key: String,
}

async fn set_key(
    State(st): State<AppState>,
    Json(body): Json<KeyBody>,
) -> Result<Json<PublicConfig>, ApiError> {
    let key = body.key.trim();
    if key.is_empty() || key.len() > 512 || key.chars().any(char::is_control) {
        return Err(ApiError::bad_request(
            "that does not look like a stream key",
        ));
    }
    st.shared
        .secrets
        .set(secret::DESTINATION_KEY, key)
        .map_err(|e| ApiError(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
    apply_destination(&st, &st.config())?;
    st.shared.config_tx.send_modify(|_| {});
    Ok(Json(public_config(&st)))
}

async fn delete_key(State(st): State<AppState>) -> Result<Json<PublicConfig>, ApiError> {
    st.shared
        .secrets
        .delete(secret::DESTINATION_KEY)
        .map_err(|e| ApiError(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
    apply_destination(&st, &st.config())?;
    st.shared.config_tx.send_modify(|_| {});
    Ok(Json(public_config(&st)))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use streamdelay_config::MemorySecrets;
    use streamdelay_relay::RelayConfig;

    use super::*;

    #[tokio::test]
    async fn dock_and_overlay_config_has_nothing_secret() {
        let relay = streamdelay_relay::start(RelayConfig {
            ingest_bind: "127.0.0.1:0".parse().unwrap(),
            ..Default::default()
        })
        .await
        .unwrap();
        let mut config = Config::default();
        config.api.token = "0123456789abcdef".into();
        config.ingest.key = Some("ingest-secret".into());
        config.obs.host = "obs.lan".into();
        let st = crate::state(
            relay,
            config,
            Arc::new(MemorySecrets::default()),
            7788,
            None,
        );

        let v = serde_json::to_value(limited_config(&st, Scope::Control)).unwrap();
        assert_eq!(v["scope"], "control");
        assert!(v["config"]["delay"]["presets"].is_array());
        assert!(v["config"]["overlay"]["mask_title"].is_string());
        assert!(v["urls"]["obs_server"].is_string());
        let text = v.to_string();
        for s in [
            "0123456789abcdef",
            "ingest-secret",
            "obs.lan",
            "token",
            "destination",
        ] {
            assert!(!text.contains(s), "limited config contains {s}: {text}");
        }

        // The dashboard sees the ingest key only as the OBS stream key.
        let v = serde_json::to_value(public_config(&st)).unwrap();
        assert_eq!(v["scope"], "admin");
        assert!(v["config"]["ingest"]["key"].is_null());
        assert_eq!(v["urls"]["obs_key"], "ingest-secret");
    }
}
