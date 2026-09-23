//! OBS setup wizard endpoints (obs-websocket).

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use streamdelay_config::{KeyMode, ObsBackup, secret};
use streamdelay_obs::{Obs, ObsError, ObsTarget, StreamSettings};
use tracing::info;

use crate::AppState;
use crate::app::{destination, urls};
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

fn target(st: &AppState) -> ObsTarget {
    let c = st.config();
    ObsTarget {
        host: c.obs.host,
        port: c.obs.port,
        password: st
            .shared
            .secrets
            .get(secret::OBS_PASSWORD)
            .filter(|p| !p.is_empty()),
    }
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
    let obs = Obs::connect(&target(&st)).await;
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
    // An empty password means "keep the saved one".
    let password = if body.password.is_empty() {
        st.shared.secrets.get(secret::OBS_PASSWORD)
    } else {
        Some(body.password.clone())
    };
    let t = ObsTarget {
        host: host.clone(),
        port: body.port,
        password: password.clone(),
    };
    let obs = Obs::connect(&t).await?;
    // Connected: remember the settings.
    if !body.password.is_empty() {
        st.shared
            .secrets
            .set(secret::OBS_PASSWORD, &body.password)
            .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    }
    change(&st, |c| {
        c.obs.host = host;
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
    f: impl FnOnce(&mut streamdelay_config::Config),
) -> Result<streamdelay_config::Config, ApiError> {
    let (config, ()) = st.change_config(f)?;
    st.shared.config_tx.send_replace(config.clone());
    Ok(config)
}

async fn configure(
    State(st): State<AppState>,
    Json(body): Json<ConfigureBody>,
) -> Result<Json<ConfigureResult>, ApiError> {
    let obs = Obs::connect(&target(&st)).await?;
    let info = obs.info().await?;
    if info.streaming {
        return Err(ObsError::Streaming.into());
    }
    let links = urls(&st);
    let mut messages = Vec::new();
    let mut imported_key = false;

    if !info.stream.points_to(&links.obs_server) {
        // Back up OBS's settings (key stored as a secret, not in the config file).
        let backup = ObsBackup {
            service_type: info.stream.service_type.clone(),
            settings_json: info.stream.without_key().to_string(),
        };
        match info.stream.key() {
            Some(k) => st.shared.secrets.set(secret::OBS_BACKUP_KEY, k),
            None => st.shared.secrets.delete(secret::OBS_BACKUP_KEY),
        }
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e))?;

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
            c.obs.backup = Some(backup);
            if imported_key {
                c.destination.key_mode = KeyMode::Stored;
                if !c.destination.url.contains("twitch.tv") {
                    c.destination.service = "twitch".into();
                    c.destination.url = streamdelay_config::SERVICES[0].url.into();
                }
            }
        })?;
        if imported_key {
            let dest = destination(
                &config,
                st.shared.secrets.as_ref(),
                st.key_override(&config.destination.url),
            );
            st.relay().set_destination(dest)?;
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
    let Some(backup) = st.config().obs.backup else {
        return Err(ApiError::bad_request(
            "there is no saved OBS configuration to restore",
        ));
    };
    let mut settings: serde_json::Value = serde_json::from_str(&backup.settings_json)
        .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if let (Some(obj), Some(key)) = (
        settings.as_object_mut(),
        st.shared.secrets.get(secret::OBS_BACKUP_KEY),
    ) {
        obj.insert("key".into(), key.into());
    }
    let obs = Obs::connect(&target(&st)).await?;
    obs.restore(&StreamSettings {
        service_type: backup.service_type,
        settings,
    })
    .await?;
    let _ = st.shared.secrets.delete(secret::OBS_BACKUP_KEY);
    change(&st, |c| c.obs.backup = None)?;
    info!("restored OBS's original stream settings");
    Ok(Json(build_status(&st, Ok(obs)).await))
}
