//! OBS setup wizard endpoints (obs-websocket).

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use streamdelay_config::{Config, KeyMode, ObsBackup, SERVICES, SecretStore, secret};
use streamdelay_obs::{Obs, ObsError, ObsTarget, StreamSettings};
use tracing::{info, warn};

use crate::AppState;
use crate::app::{same_server, urls};
use crate::routes::ApiError;

const OVERLAY_SOURCE: &str = "Stream Delay Overlay";

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/obs/status", get(status))
        .route("/api/v1/obs/connect", post(connect))
        .route("/api/v1/obs/configure", post(configure))
        .route("/api/v1/obs/restore", post(restore))
}

#[derive(Debug, Serialize, Default)]
struct ObsStatus {
    reachable: bool,
    version: Option<String>,
    streaming: bool,
    /// OBS already streams to stream-delay.
    configured: bool,
    current_server: Option<String>,
    error: Option<String>,
    has_backup: bool,
    password_saved: bool,
}

impl From<ObsError> for ApiError {
    fn from(e: ObsError) -> Self {
        let status = match e {
            ObsError::Streaming => StatusCode::CONFLICT,
            ObsError::Auth => StatusCode::UNAUTHORIZED,
            ObsError::Unreachable(_) => StatusCode::BAD_GATEWAY,
            ObsError::Other(_) => StatusCode::BAD_GATEWAY,
        };
        ApiError(status, e.to_string())
    }
}

/// The configured OBS, with its saved password.
fn target(st: &AppState, c: &Config) -> ObsTarget {
    ObsTarget {
        host: c.obs.host.clone(),
        port: c.obs.port,
        password: st
            .shared
            .secrets
            .get(secret::OBS_PASSWORD)
            .filter(|p| !p.is_empty()),
    }
}

/// Names an OBS (`host:port`), to tell whether two settings mean the same one.
fn obs_id(host: &str, port: u16) -> String {
    let host = host.trim().to_ascii_lowercase();
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Moves a backup made by an older version, which kept OBS's settings (all but
/// the stream key) in the config file, into the secret store. Returns true if
/// `config` changed.
pub(crate) fn migrate_backup(config: &mut Config, secrets: &dyn SecretStore) -> bool {
    let Some(backup) = config.obs.backup.as_mut() else {
        return false;
    };
    let Some(json) = &backup.settings_json else {
        return false;
    };
    // Made with the OBS set up now, as far as anyone can tell.
    let mut changed = backup.obs.is_none();
    backup
        .obs
        .get_or_insert_with(|| obs_id(&config.obs.host, config.obs.port));
    let mut settings: Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(e) => {
            warn!("the saved OBS settings are not valid: {e}");
            return changed;
        }
    };
    if let (Some(obj), Some(key)) = (
        settings.as_object_mut(),
        secrets.get(secret::OBS_BACKUP_KEY),
    ) {
        obj.insert("key".into(), key.into());
    }
    match secrets.set(secret::OBS_BACKUP, &settings.to_string()) {
        Ok(()) => {
            let _ = secrets.delete(secret::OBS_BACKUP_KEY);
            backup.settings_json = None;
            changed = true;
            info!("moved the saved OBS settings to the secret store");
        }
        Err(e) => warn!("could not move the saved OBS settings to the secret store: {e}"),
    }
    changed
}

/// The backed-up settings, including the stream key.
fn backup_settings(secrets: &dyn SecretStore, backup: &ObsBackup) -> Result<Value, ApiError> {
    let (json, key) = match (secrets.get(secret::OBS_BACKUP), &backup.settings_json) {
        (Some(json), _) => (json, None),
        // Not moved yet: the secret store was unavailable at startup.
        (None, Some(json)) => (json.clone(), secrets.get(secret::OBS_BACKUP_KEY)),
        (None, None) => {
            return Err(ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the saved OBS settings are missing from the secret store".into(),
            ));
        }
    };
    let mut settings: Value = serde_json::from_str(&json)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if let (Some(obj), Some(key)) = (settings.as_object_mut(), key) {
        obj.insert("key".into(), key.into());
    }
    Ok(settings)
}

/// Secret values in the saved OBS settings, for redaction.
pub(crate) fn backup_secrets(secrets: &dyn SecretStore) -> Vec<String> {
    let Some(settings) = secrets
        .get(secret::OBS_BACKUP)
        .and_then(|json| serde_json::from_str::<Value>(&json).ok())
    else {
        return Vec::new();
    };
    ["key", "password", "bearer_token"]
        .iter()
        .filter_map(|field| settings.get(field).and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

async fn build_status(st: &AppState, obs: Result<Obs, ObsError>) -> ObsStatus {
    let mut s = ObsStatus {
        has_backup: st.config().obs.backup.is_some(),
        password_saved: st.shared.secrets.get(secret::OBS_PASSWORD).is_some(),
        ..Default::default()
    };
    let result = match obs {
        Ok(obs) => obs.info().await,
        Err(e) => Err(e),
    };
    match result {
        Ok(info) => {
            s.reachable = true;
            s.version = Some(info.version);
            s.streaming = info.streaming;
            s.configured = info.stream.points_to(&urls(st).obs_server);
            s.current_server = info.stream.server();
        }
        Err(e) => s.error = Some(e.to_string()),
    }
    s
}

async fn status(State(st): State<AppState>) -> Json<ObsStatus> {
    let obs = Obs::connect(&target(&st, &st.config())).await;
    Json(build_status(&st, obs).await)
}

#[derive(Debug, Deserialize)]
struct ConnectBody {
    host: String,
    port: u16,
    #[serde(default)]
    password: String,
}

async fn connect(
    State(st): State<AppState>,
    Json(body): Json<ConnectBody>,
) -> Result<Json<ObsStatus>, ApiError> {
    let host = body.host.trim().to_string();
    if host.is_empty() {
        return Err(ApiError::bad_request(
            "enter the OBS host, usually 127.0.0.1",
        ));
    }
    let saved = st.config();
    let same_obs = obs_id(&host, body.port) == obs_id(&saved.obs.host, saved.obs.port);
    // An empty password means "keep the saved one", which only ever goes to the
    // OBS it was saved for: whoever runs another address must not receive it.
    let password = if !body.password.is_empty() {
        Some(body.password.clone())
    } else if same_obs {
        target(&st, &saved).password
    } else {
        None
    };
    let t = ObsTarget {
        host: host.clone(),
        port: body.port,
        password,
    };
    let obs = Obs::connect(&t).await?;
    // Connected: remember the settings.
    if !body.password.is_empty() {
        st.shared
            .secrets
            .set(secret::OBS_PASSWORD, &body.password)
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    } else if !same_obs {
        // The saved password belonged to the previous OBS.
        st.shared
            .secrets
            .delete(secret::OBS_PASSWORD)
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    }
    change(&st, |c| {
        c.obs.host = host.clone();
        c.obs.port = body.port;
    })?;
    Ok(Json(build_status(&st, Ok(obs)).await))
}

#[derive(Debug, Deserialize)]
struct ConfigureBody {
    #[serde(default = "yes")]
    import_key: bool,
    #[serde(default = "yes")]
    add_overlay: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Serialize)]
struct ConfigureResult {
    status: ObsStatus,
    imported_key: bool,
    overlay_added: bool,
    message: String,
}

/// Changes and saves the settings, and updates open pages.
fn change(
    st: &AppState,
    f: impl FnMut(&mut streamdelay_config::Config),
) -> Result<streamdelay_config::Config, ApiError> {
    let (config, ()) = st.change_config(f)?;
    st.shared.config_tx.send_replace(config.clone());
    Ok(config)
}

async fn configure(
    State(st): State<AppState>,
    Json(body): Json<ConfigureBody>,
) -> Result<Json<ConfigureResult>, ApiError> {
    let config = st.config();
    let obs = Obs::connect(&target(&st, &config)).await?;
    let info = obs.info().await?;
    if info.streaming {
        return Err(ObsError::Streaming.into());
    }
    let links = urls(&st);
    let mut messages = Vec::new();
    let mut imported_key = false;

    if !info.stream.points_to(&links.obs_server) {
        // Back up OBS's settings. They hold the stream key and maybe a server
        // password, so they are a secret, not part of the config file.
        let backup = ObsBackup {
            service_type: info.stream.service_type.clone(),
            obs: Some(obs_id(&config.obs.host, config.obs.port)),
            settings_json: None,
        };
        st.shared
            .secrets
            .set(secret::OBS_BACKUP, &info.stream.settings.to_string())
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e))?;
        let _ = st.shared.secrets.delete(secret::OBS_BACKUP_KEY);

        if body.import_key
            && let Some(key) = info.stream.twitch_key()
        {
            st.shared
                .secrets
                .set(secret::DESTINATION_KEY, key)
                .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            imported_key = true;
            messages.push("Your Twitch stream key was moved into stream-delay.".to_string());
        }

        let config = change(&st, |c| {
            c.obs.backup = Some(backup.clone());
            if imported_key {
                c.destination.key_mode = KeyMode::Stored;
                // A Twitch key only goes to Twitch.
                if !same_server(SERVICES[0].url, &c.destination.url) {
                    c.destination.service = "twitch".into();
                    c.destination.url = SERVICES[0].url.into();
                }
            }
        })?;
        if imported_key {
            st.relay().set_destination(st.destination(&config))?;
        }

        obs.stream_to(&links.obs_server, &links.obs_key).await?;
        info!("OBS now streams to {}", links.obs_server);
        messages.push("OBS now streams through stream-delay.".to_string());
    } else {
        messages.push("OBS already streams through stream-delay.".to_string());
    }

    let mut overlay_added = false;
    if body.add_overlay {
        overlay_added = obs
            .add_browser_source(OVERLAY_SOURCE, &links.overlay)
            .await?;
        messages.push(if overlay_added {
            format!("Added the \"{OVERLAY_SOURCE}\" browser source to your current scene.")
        } else {
            format!("The \"{OVERLAY_SOURCE}\" source already exists.")
        });
    }
    messages
        .push("Next: add the dock (Docks → Custom Browser Docks) with the Dock URL below.".into());

    let status = build_status(&st, Ok(obs)).await;
    Ok(Json(ConfigureResult {
        status,
        imported_key,
        overlay_added,
        message: messages.join(" "),
    }))
}

async fn restore(State(st): State<AppState>) -> Result<Json<ObsStatus>, ApiError> {
    let config = st.config();
    let Some(backup) = config.obs.backup.clone() else {
        return Err(ApiError::bad_request(
            "there is no saved OBS configuration to restore",
        ));
    };
    // The settings hold a stream key: they only go back to the OBS they came from.
    if let Some(from) = &backup.obs
        && *from != obs_id(&config.obs.host, config.obs.port)
    {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!(
                "the saved settings are from the OBS at {from}; connect to that OBS to restore them"
            ),
        ));
    }
    let settings = backup_settings(st.shared.secrets.as_ref(), &backup)?;
    let obs = Obs::connect(&target(&st, &config)).await?;
    obs.restore(&StreamSettings {
        service_type: backup.service_type,
        settings,
    })
    .await?;
    let _ = st.shared.secrets.delete(secret::OBS_BACKUP);
    let _ = st.shared.secrets.delete(secret::OBS_BACKUP_KEY);
    change(&st, |c| c.obs.backup = None)?;
    info!("restored OBS's original stream settings");
    Ok(Json(build_status(&st, Ok(obs)).await))
}

#[cfg(test)]
mod tests {
    use streamdelay_config::MemorySecrets;

    use super::*;

    #[test]
    fn old_backups_move_to_the_secret_store() {
        let secrets = MemorySecrets::default();
        secrets.set(secret::OBS_BACKUP_KEY, "live_1_abc").unwrap();
        let mut config = Config::default();
        config.obs.port = 4456;
        config.obs.backup = Some(ObsBackup {
            service_type: "rtmp_custom".into(),
            obs: None,
            settings_json: Some(r#"{"server":"rtmp://x/app","password":"pw"}"#.into()),
        });
        assert!(migrate_backup(&mut config, &secrets));
        let backup = config.obs.backup.clone().unwrap();
        assert_eq!(backup.settings_json, None);
        assert_eq!(backup.obs.as_deref(), Some("127.0.0.1:4456"));
        assert_eq!(secrets.get(secret::OBS_BACKUP_KEY), None);
        let settings = backup_settings(&secrets, &backup).unwrap();
        assert_eq!(settings["key"], "live_1_abc");
        assert_eq!(settings["password"], "pw");
        assert_eq!(backup_secrets(&secrets), vec!["live_1_abc", "pw"]);
        assert!(!migrate_backup(&mut config, &secrets), "only once");
    }

    #[test]
    fn obs_ids_compare_hosts_loosely() {
        assert_eq!(obs_id(" LocalHost ", 4455), obs_id("localhost", 4455));
        assert_ne!(obs_id("127.0.0.1", 4455), obs_id("127.0.0.1", 4456));
        assert_eq!(obs_id("::1", 4455), "[::1]:4455");
        assert_eq!(obs_id("[::1]", 4455), "[::1]:4455");
    }
}
