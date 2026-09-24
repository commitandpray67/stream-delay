//! Settings endpoints: read and change configuration, and store the stream key.

use std::sync::atomic::Ordering;

use axum::extract::State;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use streamdelay_config::{
    Config, DelayConfig, DestinationConfig, HotkeyConfig, OverlayConfig, SERVICES,
};
use streamdelay_relay::RtmpUrl;
use tracing::info;

use crate::AppState;
use crate::app::{Urls, different_server, split_url_key, urls};
use crate::auth::Scope;
use crate::changes::KeyChange;
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

/// What applying a [`SettingsUpdate`] changed that the settings file does not
/// cover.
struct Applied {
    keep_buffer_changed: bool,
    /// Takes effect at the next start.
    restart: bool,
}

impl SettingsUpdate {
    fn apply(&self, c: &mut Config) -> Applied {
        let mut applied = Applied {
            keep_buffer_changed: false,
            restart: false,
        };
        if let Some(d) = &self.destination {
            c.destination = d.clone();
        }
        if let Some(d) = &self.delay {
            applied.restart |=
                d.max_seconds != c.delay.max_seconds || d.ram_cap_mb != c.delay.ram_cap_mb;
            applied.keep_buffer_changed = d.keep_buffer != c.delay.keep_buffer;
            c.delay = d.clone();
        }
        if let Some(o) = &self.overlay {
            c.overlay = o.clone();
        }
        if let Some(h) = &self.hotkeys {
            c.hotkeys = h.clone();
        }
        if let Some(g) = self.grace_seconds {
            applied.restart |= g != c.ingest.grace_seconds;
            c.ingest.grace_seconds = g;
        }
        if let Some(l) = self.allow_lan {
            applied.restart |= l != c.api.allow_lan;
            c.api.allow_lan = l;
        }
        applied
    }
}

async fn update_config(
    State(st): State<AppState>,
    Json(mut update): Json<SettingsUpdate>,
) -> Result<Json<PublicConfig>, ApiError> {
    // One change at a time, from reading the settings to the relay: a concurrent
    // one could otherwise pair a key with the wrong destination.
    let lock = st.lock_settings();
    validate(&update, &st.config()).map_err(ApiError::bad_request)?;
    let mut key = KeyChange::Keep;
    if let Some(d) = &mut update.destination {
        // A key typed into the URL (rtmp://host/app/<key>) is stored like one
        // entered in the key field, so the URL shown to the UI and saved in
        // config.toml never contains it.
        let (url, url_key) = split_url_key(&d.url);
        d.url = url;
        if let Some(k) = url_key {
            key = KeyChange::Save {
                server: d.url.clone(),
                key: k,
            };
        } else if different_server(&st.saved_destination_url(), &d.url) {
            // The saved key belongs to the destination in the settings file (the
            // one in effect may be a command-line override), and is never sent to
            // another server: it is forgotten too, and if that fails, nothing
            // changes.
            key = KeyChange::Forget;
        }
    }
    let forget = matches!(key, KeyChange::Forget);
    let (config, applied) = st.change_settings_locked(&lock, key, |c| update.apply(c))?;
    if forget {
        info!("destination server changed; the stored stream key was removed");
    }
    if applied.restart {
        st.shared.restart_required.store(true, Ordering::Relaxed);
    }
    if applied.keep_buffer_changed {
        st.relay().set_keep_history(config.delay.keep_buffer)?;
    }
    drop(lock);
    Ok(Json(public_config(&st)))
}

#[derive(Debug, Deserialize)]
struct KeyBody {
    key: String,
}

async fn set_key(
    State(st): State<AppState>,
    Json(body): Json<KeyBody>,
) -> Result<Json<PublicConfig>, ApiError> {
    st.save_destination_key(&body.key)?;
    Ok(Json(public_config(&st)))
}

async fn delete_key(State(st): State<AppState>) -> Result<Json<PublicConfig>, ApiError> {
    st.forget_destination_key()?;
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

#[cfg(test)]
mod transaction_tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use streamdelay_config::{MemorySecrets, SecretStore, secret};
    use streamdelay_relay::{DestinationKey, RelayConfig};
    use tower::ServiceExt;

    use super::*;

    const TOKEN: &str = "0123456789abcdef";

    async fn setup(
        path: Option<std::path::PathBuf>,
    ) -> (AppState, axum::Router, Arc<MemorySecrets>) {
        let relay = streamdelay_relay::start(RelayConfig {
            ingest_bind: "127.0.0.1:0".parse().unwrap(),
            ..Default::default()
        })
        .await
        .unwrap();
        let mut config = Config::default();
        config.api.token = TOKEN.into();
        let secrets = Arc::new(MemorySecrets::default());
        let st = crate::state(relay, config, secrets.clone(), 7788, path);
        (st.clone(), crate::routes::router(st), secrets)
    }

    async fn call(
        app: &axum::Router,
        method: &str,
        path: &str,
        body: &str,
    ) -> (StatusCode, String) {
        let r = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "127.0.0.1:7788")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.clone().oneshot(r).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).into())
    }

    fn dest(url: &str) -> String {
        format!(r#"{{"destination":{{"service":"custom","url":"{url}","key_mode":"stored"}}}}"#)
    }

    fn applied_key(st: &AppState) -> Option<String> {
        match st.shared.applied_destination.lock().unwrap().clone()?.key {
            DestinationKey::Fixed(k) => Some(k),
            DestinationKey::Passthrough => None,
        }
    }

    #[tokio::test]
    async fn a_new_key_in_the_same_url_reaches_the_relay() {
        let (st, app, _) = setup(None).await;
        let (s, _) = call(
            &app,
            "PUT",
            "/api/v1/config",
            &dest("rtmp://live.twitch.tv/app/live_1_a"),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(applied_key(&st).as_deref(), Some("live_1_a"));
        // Only the key changes: the address in the settings stays the same.
        let (s, _) = call(
            &app,
            "PUT",
            "/api/v1/config",
            &dest("rtmp://live.twitch.tv/app/live_1_b"),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(applied_key(&st).as_deref(), Some("live_1_b"));
        // And through the key field.
        let (s, _) = call(
            &app,
            "PUT",
            "/api/v1/destination/key",
            r#"{"key":"live_1_c"}"#,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(applied_key(&st).as_deref(), Some("live_1_c"));
        let (s, _) = call(&app, "DELETE", "/api/v1/destination/key", "").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(applied_key(&st).as_deref(), Some(""));
    }

    #[tokio::test]
    async fn a_change_that_cannot_be_saved_keeps_the_key_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let (st, app, secrets) = setup(Some(path.clone())).await;
        let (s, _) = call(
            &app,
            "PUT",
            "/api/v1/destination/key",
            r#"{"key":"live_1_a"}"#,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let saved = secrets.get(secret::DESTINATION_KEY).unwrap();
        // From now on the settings file cannot be written.
        std::fs::create_dir(dir.path().join("config.toml.tmp")).unwrap();
        for update in [
            // Another server: the key would be forgotten.
            dest("rtmp://ingest.example.net/live"),
            // A new key in the URL: it would replace the saved one.
            dest("rtmp://live.twitch.tv/app/live_1_b"),
        ] {
            let (s, body) = call(&app, "PUT", "/api/v1/config", &update).await;
            assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
            assert!(body.contains("nothing was changed"), "{body}");
            assert_eq!(
                secrets.get(secret::DESTINATION_KEY).as_deref(),
                Some(saved.as_str())
            );
            assert_eq!(st.config().destination.url, "rtmp://live.twitch.tv/app");
            assert_eq!(applied_key(&st).as_deref(), Some("live_1_a"));
        }
    }
}
