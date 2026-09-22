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
use tracing::{info, warn};

use crate::AppState;
use crate::app::{Urls, destination, urls};
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

/// Configuration as shown to the UI: no token, no secrets.
#[derive(Debug, Serialize)]
pub(crate) struct PublicConfig {
    config: Config,
    destination_key_set: bool,
    secrets_backend: String,
    urls: Urls,
    services: Vec<ServiceInfo>,
    restart_required: bool,
    version: &'static str,
}

pub(crate) fn public_config(st: &AppState) -> PublicConfig {
    let mut config = st.config();
    config.api.token = String::new();
    let key_set = st.shared.key_override.is_some()
        || st
            .shared
            .secrets
            .get(secret::DESTINATION_KEY)
            .is_some_and(|k| !k.is_empty());
    PublicConfig {
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

fn validate(u: &SettingsUpdate) -> Result<(), String> {
    if let Some(d) = &u.destination
        && !d.url.trim().is_empty()
    {
        RtmpUrl::parse(&d.url).map_err(|e| format!("destination URL: {e}"))?;
    }
    if let Some(d) = &u.delay {
        if !(5..=900).contains(&d.max_seconds) {
            return Err("maximum delay must be between 5 and 900 seconds".into());
        }
        if d.presets.is_empty() || d.presets.len() > 10 {
            return Err("configure between 1 and 10 presets".into());
        }
        let max = d.max_seconds as f64;
        for p in &d.presets {
            if !p.seconds.is_finite() || p.seconds < 0.0 || p.seconds > max {
                return Err(format!("preset {} s is outside 0..{max} s", p.seconds));
            }
        }
        if !(0.0..=max).contains(&d.start_seconds) {
            return Err("start delay must be between 0 and the maximum delay".into());
        }
        if !(16..=16_384).contains(&d.ram_cap_mb) {
            return Err("memory cap must be between 16 and 16384 MiB".into());
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
    Json(update): Json<SettingsUpdate>,
) -> Result<Json<PublicConfig>, ApiError> {
    validate(&update).map_err(ApiError::bad_request)?;
    let mut destination_changed = false;
    let new_config = {
        let mut c = st.shared.config.write().expect("config lock");
        let mut restart = false;
        if let Some(d) = update.destination {
            destination_changed = d != c.destination;
            c.destination = d;
        }
        if let Some(d) = update.delay {
            restart |= d.max_seconds != c.delay.max_seconds || d.ram_cap_mb != c.delay.ram_cap_mb;
            c.delay = d;
        }
        if let Some(o) = update.overlay {
            c.overlay = o;
        }
        if let Some(h) = update.hotkeys {
            c.hotkeys = h;
        }
        if let Some(g) = update.grace_seconds {
            restart |= g != c.ingest.grace_seconds;
            c.ingest.grace_seconds = g;
        }
        if let Some(l) = update.allow_lan {
            restart |= l != c.api.allow_lan;
            c.api.allow_lan = l;
        }
        if restart {
            st.shared.restart_required.store(true, Ordering::Relaxed);
        }
        c.clone()
    };
    save(&st, &new_config)?;
    if destination_changed {
        apply_destination(&st, &new_config)?;
    }
    st.shared.config_tx.send_replace(new_config);
    Ok(Json(public_config(&st)))
}

fn save(st: &AppState, c: &Config) -> Result<(), ApiError> {
    if let Some(path) = &st.shared.config_path {
        c.save(path).map_err(|e| {
            warn!("saving settings failed: {e}");
            ApiError(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;
    }
    Ok(())
}

fn apply_destination(st: &AppState, c: &Config) -> Result<(), ApiError> {
    let dest = destination(
        c,
        st.shared.secrets.as_ref(),
        st.shared.key_override.as_deref(),
    );
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
