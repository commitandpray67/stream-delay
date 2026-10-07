//! Settings endpoints: read and change configuration, and store the stream key.

use std::sync::atomic::Ordering;

use axum::extract::State;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use streamdelay_config::{
    AfterReconnect, Config, DelayConfig, DestinationConfig, HotkeyConfig, OverlayConfig, SERVICES,
};
use streamdelay_relay::RtmpUrl;
use tracing::info;

use crate::AppState;
use crate::app::{Urls, different_server, shown_url, split_url_key, urls};
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
    /// Encoders must stream with the ingest key (`urls.obs_key`); with
    /// passthrough, that is the key forwarded to the destination.
    ingest_key_required: bool,
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
    let ingest_key_required = config.ingest.key.as_deref().is_some_and(|k| !k.is_empty());
    // Shown as `urls.obs_key` instead, which diagnostics leave out.
    config.ingest.key = None;
    let key_url = split_url_key(&config.destination.url).0;
    // Keys are moved out of the URL when it is saved; never show one regardless,
    // nor the values of its query.
    config.destination.url = shown_url(&config.destination.url);
    // Only there until moved to the secret store; may hold a server password.
    if let Some(backup) = &mut config.obs.backup {
        backup.settings_json = None;
    }
    let key_set = st.key_override(&key_url).is_some() || st.stored_key(&key_url).is_some();
    PublicConfig {
        scope: Scope::Admin,
        config,
        destination_key_set: key_set,
        ingest_key_required,
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

/// What stands for a hidden part of a URL as shown (see [`shown_url`]).
const HIDDEN: char = '…';

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
    if !(500..=5_000).contains(&d.mask_margin_ms) {
        return Err("the slate margin must be between 500 and 5000 ms".into());
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
        let url = RtmpUrl::parse(&d.url).map_err(|e| format!("destination URL: {e}"))?;
        // The URL as shown, but edited: what `…` stood for is not known, and
        // saving `…` in its place would lose it.
        if url
            .stream_key
            .as_deref()
            .is_some_and(|k| k.contains(HIDDEN))
        {
            return Err(
                "destination URL: the stream key after the application is hidden (…): enter it \
                 again, in the URL or the key field"
                    .into(),
            );
        }
        if url.query().is_some_and(|q| q.contains(HIDDEN)) {
            return Err("destination URL: what follows the ? is hidden (…): enter it again".into());
        }
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
    // Also when only the presets change: keys for presets that did not exist
    // were not counted, and are once those presets do.
    if u.hotkeys.is_some() || u.delay.is_some() {
        let presets = u.delay.as_ref().unwrap_or(&current.delay).presets.len();
        check_hotkeys(u.hotkeys.as_ref().unwrap_or(&current.hotkeys), presets)?;
    }
    if let Some(o) = &u.overlay {
        for c in [&o.accent_color, &o.background_color, &o.text_color] {
            if !valid_color(c) {
                return Err(format!("'{c}' is not a hex color like #9147ff"));
            }
        }
        // In characters, as the dashboard's fields count them.
        let chars = |s: &str| s.chars().count();
        if chars(&o.mask_title) > 200
            || chars(&o.mask_subtitle) > 400
            || chars(&o.mask_image) > 2048
        {
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

/// Refuses one hotkey for two actions: the desktop app would give it to the
/// first it registers and ignore the other, so a key meant to dump the
/// buffer could end the stream. `presets` is how many presets there are (keys
/// for others are not registered).
fn check_hotkeys(h: &HotkeyConfig, presets: usize) -> Result<(), String> {
    let actions = [
        ("Remove delay now", &h.go_live),
        ("Remove delay after it airs", &h.go_live_after_air),
        ("End stream now", &h.end_stream),
        (
            "End stream (after the buffer airs)",
            &h.end_stream_after_air,
        ),
        ("Dump buffer", &h.dump),
    ]
    .map(|(name, spec)| (name.to_string(), spec));
    let presets = h
        .presets
        .iter()
        .take(presets)
        .enumerate()
        .map(|(i, spec)| (format!("Preset {}", i + 1), spec));
    let mut seen: Vec<(String, String)> = Vec::new();
    for (name, spec) in actions.into_iter().chain(presets) {
        let Some(keys) = hotkey_keys(spec) else {
            continue;
        };
        if let Some((_, other)) = seen.iter().find(|(k, _)| *k == keys) {
            return Err(format!(
                "{other} and {name} have the same hotkey ({}): give each its own",
                spec.trim()
            ));
        }
        seen.push((keys, name));
    }
    Ok(())
}

/// A hotkey as the desktop app reads it, so that two ways of writing the same
/// keys compare equal: case, spaces, the order of the modifiers and their other
/// names (`Option` for `Alt`, `Digit1` for `1`) make no difference. `None` for
/// an unassigned one.
fn hotkey_keys(spec: &str) -> Option<String> {
    let mut parts: Vec<String> = spec
        .split('+')
        .map(|p| p.trim().to_ascii_uppercase())
        .collect();
    let key = parts.pop().filter(|k| !k.is_empty())?;
    // As the desktop app's shortcut library reads it.
    let cmd_or_ctrl = if cfg!(target_os = "macos") {
        "SUPER"
    } else {
        "CONTROL"
    };
    let mut mods: Vec<&str> = parts
        .iter()
        .map(|m| match m.as_str() {
            "OPTION" | "ALT" => "ALT",
            "CONTROL" | "CTRL" => "CONTROL",
            "COMMAND" | "CMD" | "SUPER" => "SUPER",
            "COMMANDORCONTROL" | "COMMANDORCTRL" | "CMDORCTRL" | "CMDORCONTROL" => cmd_or_ctrl,
            other => other,
        })
        .collect();
    mods.sort_unstable();
    mods.dedup();
    let key = ["DIGIT", "KEY"]
        .iter()
        .find_map(|p| key.strip_prefix(p).filter(|k| k.len() == 1))
        .map_or(key.clone(), str::to_string);
    Some(format!("{}+{key}", mods.join("+")))
}

/// What applying a [`SettingsUpdate`] changed that the settings file does not
/// cover.
struct Applied {
    keep_buffer_changed: bool,
    after_reconnect_changed: bool,
    /// Takes effect at the next start.
    restart: bool,
}

impl SettingsUpdate {
    fn apply(&self, c: &mut Config) -> Applied {
        let mut applied = Applied {
            keep_buffer_changed: false,
            after_reconnect_changed: false,
            restart: false,
        };
        if let Some(d) = &self.destination {
            c.destination = d.clone();
        }
        if let Some(d) = &self.delay {
            applied.restart |= d.max_seconds != c.delay.max_seconds
                || d.ram_cap_mb != c.delay.ram_cap_mb
                || d.mask_margin_ms != c.delay.mask_margin_ms
                || d.mute_under_slate != c.delay.mute_under_slate;
            applied.keep_buffer_changed = d.keep_buffer != c.delay.keep_buffer;
            applied.after_reconnect_changed = d.after_reconnect != c.delay.after_reconnect;
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
    if let Some(d) = &mut update.destination {
        // The URL as shown, its query hidden, stands for the saved one.
        let saved = st.saved_destination_url();
        if d.url.trim() != saved && d.url.trim() == shown_url(&saved) {
            d.url = saved;
        }
    }
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
    if applied.after_reconnect_changed {
        st.relay()
            .set_restore_after_reconnect(config.delay.after_reconnect == AfterReconnect::Restore)?;
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

    #[test]
    fn only_settings_read_at_startup_ask_for_a_restart() {
        let current = Config::default();
        let apply = |v: serde_json::Value| {
            let u: SettingsUpdate = serde_json::from_value(v).unwrap();
            let a = u.apply(&mut current.clone());
            (a.restart, a.keep_buffer_changed)
        };
        let delay = |key: &str, value: serde_json::Value| {
            let mut d = serde_json::to_value(&current.delay).unwrap();
            d[key] = value;
            serde_json::json!({ "delay": d })
        };
        // Read when the relay starts: the buffer's size, and the grace period.
        assert_eq!(apply(delay("max_seconds", 60.into())), (true, false));
        assert_eq!(apply(delay("ram_cap_mb", 256.into())), (true, false));
        assert_eq!(apply(delay("mask_margin_ms", 2_500.into())), (true, false));
        assert_eq!(
            apply(delay("mute_under_slate", false.into())),
            (true, false)
        );
        assert_eq!(
            apply(serde_json::json!({ "grace_seconds": 45 })),
            (true, false)
        );
        assert_eq!(
            apply(serde_json::json!({ "allow_lan": true })),
            (true, false)
        );
        // In effect right away.
        assert_eq!(apply(delay("keep_buffer", false.into())), (false, true));
        let u: SettingsUpdate =
            serde_json::from_value(delay("after_reconnect", "restore".into())).unwrap();
        let mut c = current.clone();
        let a = u.apply(&mut c);
        assert!(!a.restart && a.after_reconnect_changed);
        assert_eq!(c.delay.after_reconnect, AfterReconnect::Restore);
        // Only ever read when stream-delay starts, which is what it is for: no
        // restart to ask for.
        assert_eq!(apply(delay("start_seconds", 5.into())), (false, false));
        let same_grace = current.ingest.grace_seconds;
        assert_eq!(
            apply(serde_json::json!({ "grace_seconds": same_grace, "allow_lan": false })),
            (false, false)
        );
    }

    #[test]
    fn settings_are_validated_to_their_limits() {
        let current = Config::default();
        let check = |v: serde_json::Value| {
            let u: SettingsUpdate = serde_json::from_value(v).unwrap();
            validate(&u, &current)
        };
        let overlay = |key: &str, value: serde_json::Value| {
            let mut o = serde_json::to_value(&current.overlay).unwrap();
            o[key] = value;
            serde_json::json!({ "overlay": o })
        };
        // Colors: hex only.
        for ok in ["#9147ff", "#fff", "#9147ffcc", "#ABCDEF"] {
            assert!(check(overlay("accent_color", ok.into())).is_ok(), "{ok}");
        }
        for bad in ["9147ff", "#12345", "#ggg", "red", "#fff;x:y", "", "#"] {
            assert!(check(overlay("text_color", bad.into())).is_err(), "{bad}");
        }
        // Overlay text, to the character.
        for (key, max) in [
            ("mask_title", 200),
            ("mask_subtitle", 400),
            ("mask_image", 2048),
        ] {
            assert!(check(overlay(key, "x".repeat(max).into())).is_ok(), "{key}");
            assert!(
                check(overlay(key, "x".repeat(max + 1).into())).is_err(),
                "{key}"
            );
            // Characters, as the fields count them, not bytes: a title in
            // another script is not refused at half the length.
            assert!(check(overlay(key, "é".repeat(max).into())).is_ok(), "{key}");
            assert!(
                check(overlay(key, "é".repeat(max + 1).into())).is_err(),
                "{key}"
            );
        }
        // Between 1 and 10 presets, each from 0 to the maximum delay.
        let presets = |seconds: Vec<f64>| {
            let mut d = serde_json::to_value(&current.delay).unwrap();
            let one = d["presets"][0].clone();
            d["presets"] = seconds
                .into_iter()
                .map(|s| {
                    let mut p = one.clone();
                    p["seconds"] = s.into();
                    p
                })
                .collect();
            serde_json::json!({ "delay": d })
        };
        let max = current.delay.max_seconds as f64;
        assert!(check(presets(vec![5.0; 10])).is_ok());
        assert!(check(presets(vec![])).is_err());
        assert!(check(presets(vec![5.0; 11])).is_err());
        assert!(check(presets(vec![0.0, max])).is_ok());
        assert!(check(presets(vec![-0.5])).is_err());
        assert!(check(presets(vec![max + 0.5])).is_err());
        // The slate margin.
        let margin = |ms: u64| {
            let mut d = serde_json::to_value(&current.delay).unwrap();
            d["mask_margin_ms"] = ms.into();
            check(serde_json::json!({ "delay": d }))
        };
        assert!(margin(500).is_ok() && margin(5_000).is_ok());
        assert!(margin(499).is_err() && margin(5_001).is_err());
        // The encoder grace period.
        assert!(check(serde_json::json!({ "grace_seconds": 600 })).is_ok());
        assert!(check(serde_json::json!({ "grace_seconds": 601 })).is_err());
    }

    #[test]
    fn one_hotkey_is_never_given_two_actions() {
        let current = Config::default();
        let check = |edit: &dyn Fn(&mut HotkeyConfig)| {
            let mut h = current.hotkeys.clone();
            edit(&mut h);
            let u: SettingsUpdate =
                serde_json::from_value(serde_json::json!({ "hotkeys": h })).unwrap();
            validate(&u, &current)
        };
        assert!(check(&|_| {}).is_ok());
        assert!(check(&|h| h.dump = "Ctrl+Alt+D".into()).is_ok());
        // The app would give the key to one of them, silently: here the
        // stream would end instead of the buffer being dumped.
        let e = check(&|h| {
            h.end_stream = "Ctrl+Alt+D".into();
            h.dump = "Ctrl+Alt+D".into();
        })
        .unwrap_err();
        assert!(e.contains("Ctrl+Alt+D"), "{e}");
        // However it is written.
        for same in ["ctrl + alt + d", "Alt+Control+D", "Ctrl+Option+KeyD"] {
            let r = check(&|h| {
                h.end_stream = "Ctrl+Alt+D".into();
                h.dump = same.into();
            });
            assert!(r.is_err(), "{same}");
        }
        // A preset's key, as another action's.
        assert!(check(&|h| h.dump = "CmdOrCtrl+Alt+Shift+1".into()).is_err());
        assert!(check(&|h| h.presets[1] = "Shift+Alt+CmdOrCtrl+Digit1".into()).is_err());
        // Unassigned ones, and keys for presets that do not exist, do not count.
        assert!(check(&|h| h.presets.push("CmdOrCtrl+Alt+Shift+L".into())).is_ok());
        // Other keys, or other modifiers, are other hotkeys.
        assert!(check(&|h| h.dump = "CmdOrCtrl+Alt+Shift+9".into()).is_ok());
        assert!(check(&|h| h.dump = "CmdOrCtrl+Alt+1".into()).is_ok());

        // The fifth preset was removed and its key given to Dump: adding a fifth
        // preset back on the Delay tab would give that key two actions.
        let mut current = Config::default();
        current.delay.presets.truncate(4);
        current.hotkeys.dump = current.hotkeys.presets[4].clone();
        let mut delay = current.delay.clone();
        delay.presets.push(streamdelay_config::Preset {
            seconds: 90.0,
            mode: delay.default_mode,
        });
        let u: SettingsUpdate =
            serde_json::from_value(serde_json::json!({ "delay": delay })).unwrap();
        let e = validate(&u, &current).unwrap_err();
        assert!(e.contains("Dump buffer") && e.contains("Preset 5"), "{e}");
    }

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
    async fn credentials_in_the_url_query_are_never_shown() {
        const SENTINEL: &str = "SENTINEL_credential_123";
        let (st, app, _) = setup(None).await;
        let url = format!("rtmps://ingest.example/live?auth={SENTINEL}");
        let (s, body) = call(&app, "PUT", "/api/v1/config", &dest(&url)).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert!(!body.contains(SENTINEL), "{body}");
        assert!(body.contains("rtmps://ingest.example/live?…"), "{body}");
        assert_eq!(st.config().destination.url, url);
        // Saving the settings as shown keeps the query.
        let (s, body) = call(
            &app,
            "PUT",
            "/api/v1/config",
            &dest("rtmps://ingest.example/live?…"),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert_eq!(st.config().destination.url, url);
        // The relay takes it up in its own time.
        for _ in 0..100 {
            if st.relay().state().egress.destination.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // Every view of the settings and state.
        for path in ["/api/v1/config", "/api/v1/state", "/api/v1/diagnostics"] {
            let (s, body) = call(&app, "GET", path, "").await;
            assert_eq!(s, StatusCode::OK, "{path}");
            assert!(!body.contains(SENTINEL), "{path}: {body}");
        }
        let state = st.relay().state();
        assert_eq!(
            state.egress.destination.as_deref(),
            Some("rtmps://ingest.example/live?…")
        );
        let applied = st.shared.applied_destination.lock().unwrap().clone();
        assert!(!format!("{applied:?}").contains(SENTINEL));
        // Another query is a change like any other.
        let other = "rtmps://ingest.example/live?auth=other";
        let (s, _) = call(&app, "PUT", "/api/v1/config", &dest(other)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(st.config().destination.url, other);
    }

    #[tokio::test]
    async fn a_hidden_key_or_query_is_never_saved_in_its_place() {
        let (st, app, secrets) = setup(None).await;
        let url = "rtmps://ingest.example/live?auth=SENTINEL_credential_123";
        let (s, _) = call(&app, "PUT", "/api/v1/config", &dest(url)).await;
        assert_eq!(s, StatusCode::OK);
        // The URL as shown, edited elsewhere: what `…` stood for is not known.
        for edited in [
            "rtmps://other.example/live?…",
            "rtmp://ingest.example:1935/live/…",
        ] {
            let (s, body) = call(&app, "PUT", "/api/v1/config", &dest(edited)).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{edited}: {body}");
            assert!(body.contains("again"), "{body}");
            assert_eq!(st.config().destination.url, url);
            assert_eq!(secrets.get(secret::DESTINATION_KEY), None);
        }
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
            dest("rtmps://live.twitch.tv:443/app/live_1_b"),
        ] {
            let (s, body) = call(&app, "PUT", "/api/v1/config", &update).await;
            assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
            assert!(body.contains("nothing was changed"), "{body}");
            assert_eq!(
                secrets.get(secret::DESTINATION_KEY).as_deref(),
                Some(saved.as_str())
            );
            assert_eq!(
                st.config().destination.url,
                "rtmps://live.twitch.tv:443/app"
            );
            assert_eq!(applied_key(&st).as_deref(), Some("live_1_a"));
        }
    }
}
