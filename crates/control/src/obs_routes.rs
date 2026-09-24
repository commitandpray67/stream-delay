//! OBS setup wizard endpoints (obs-websocket).

use std::net::{IpAddr, Ipv6Addr, SocketAddr};

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use streamdelay_config::{Config, KeyMode, ObsBackup, SERVICES, SecretStore, secret};
use streamdelay_obs::{Obs, ObsError, ObsTarget, SourceChange, StreamSettings};
use streamdelay_relay::RtmpUrl;
use tracing::{info, warn};

use crate::AppState;
use crate::app::{LOCAL_KEY, same_server, urls};
use crate::auth::Scope;
use crate::bound::{self, Saved};
use crate::changes::KeyChange;
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

/// The password saved for the OBS at `host:port`, if any. One saved by an older
/// version, on its own, belongs to the OBS in the settings.
fn password_for(secrets: &dyn SecretStore, c: &Config) -> Option<String> {
    let id = obs_id(&c.obs.host, c.obs.port);
    bound::load_for::<String>(secrets, secret::OBS_PASSWORD, &id, &id, Some)
        .filter(|p| !p.is_empty())
}

/// The saved OBS password, whichever OBS it is for (for redaction).
pub(crate) fn saved_password(secrets: &dyn SecretStore) -> Option<String> {
    bound::load::<String>(secrets, secret::OBS_PASSWORD, Some).map(|s| s.value)
}

/// The saved backup of OBS's settings, and the OBS it belongs to if recorded.
fn saved_backup(secrets: &dyn SecretStore) -> Option<Saved<Value>> {
    bound::load(secrets, secret::OBS_BACKUP, |json| {
        serde_json::from_str(&json).ok()
    })
}

/// The configured OBS, with its saved password.
fn target(st: &AppState, c: &Config) -> ObsTarget {
    ObsTarget {
        host: c.obs.host.clone(),
        port: c.obs.port,
        password: password_for(st.shared.secrets.as_ref(), c),
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

/// True for an OBS on this computer: `localhost`, a loopback address, or one of
/// this computer's own network addresses (OBS's WebSocket settings show the
/// computer's LAN address, so that is what people often enter).
async fn is_local(host: &str) -> bool {
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let ips: Vec<IpAddr> = match host.parse::<IpAddr>() {
        Ok(ip) => vec![ip],
        Err(_) => match tokio::net::lookup_host((host, 0)).await {
            Ok(addrs) => addrs.map(|a| a.ip()).collect(),
            Err(_) => return false,
        },
    };
    // Binding only works to an address this computer has.
    ips.into_iter().any(|ip| {
        ip.is_loopback() || (!ip.is_unspecified() && std::net::UdpSocket::bind((ip, 0)).is_ok())
    })
}

/// This computer's address as seen from `host`: the one the OS sends from to
/// reach it. Nothing is sent.
async fn local_ip_towards(host: &str, port: u16) -> Option<IpAddr> {
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    let to = tokio::net::lookup_host((host, port)).await.ok()?.next()?;
    let any = if to.is_ipv4() {
        SocketAddr::from(([0, 0, 0, 0], 0))
    } else {
        SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))
    };
    let socket = tokio::net::UdpSocket::bind(any).await.ok()?;
    socket.connect(to).await.ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}

/// The server address the OBS in `c` should stream to. For an OBS on another
/// computer, `127.0.0.1` would be that computer itself.
async fn server_for_obs(st: &AppState, c: &Config) -> Result<String, ApiError> {
    if is_local(&c.obs.host).await {
        return Ok(urls(st).obs_server);
    }
    let ingest = st.relay().ingest_addr();
    let ip = if ingest.ip().is_loopback() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "The OBS address on this tab belongs to another computer, but stream-delay only \
             accepts streams from this one. If OBS runs on this computer, enter 127.0.0.1 as \
             its address. If it really runs on another computer, start stream-delay with \
             --ingest 0.0.0.0:1935 (see Two-PC setups in the user guide)."
                .into(),
        ));
    } else if ingest.ip().is_unspecified() {
        local_ip_towards(&c.obs.host, c.obs.port)
            .await
            .ok_or_else(|| {
                ApiError(
                    StatusCode::BAD_GATEWAY,
                    format!(
                        "could not find this computer's address on the way to {}",
                        c.obs.host
                    ),
                )
            })?
    } else {
        ingest.ip()
    };
    Ok(format!(
        "rtmp://{}/live",
        SocketAddr::new(ip, ingest.port())
    ))
}

/// The overlay's address for the OBS in `c`. For an OBS on another computer,
/// `127.0.0.1` would be that computer itself, and the page is only served to it
/// if stream-delay accepts requests from the network.
async fn overlay_for_obs(st: &AppState, c: &Config) -> Result<String, ApiError> {
    if is_local(&c.obs.host).await {
        return Ok(urls(st).overlay);
    }
    let bind = c.api.bind.ip();
    if bind.is_loopback() || !st.shared.allow_lan {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "OBS runs on another computer, which would load the overlay from this one, but \
             stream-delay's web pages are only served to this computer. Start stream-delay \
             with --api 0.0.0.0:7788 --allow-lan (see Two-PC setups in the user guide), or \
             set up OBS without the overlay."
                .into(),
        ));
    }
    let ip = if bind.is_unspecified() {
        local_ip_towards(&c.obs.host, c.obs.port)
            .await
            .ok_or_else(|| {
                ApiError(
                    StatusCode::BAD_GATEWAY,
                    format!(
                        "could not find this computer's address on the way to {}",
                        c.obs.host
                    ),
                )
            })?
    } else {
        bind
    };
    Ok(format!(
        "http://{}/overlay?token={}",
        SocketAddr::new(ip, st.shared.port),
        st.shared.tokens.get(Scope::Read)
    ))
}

/// True if OBS streams to a stream-delay already, for example at an earlier
/// address of this one (its port changed): then OBS's settings are not the
/// user's own, and backing them up would replace the backup that is.
fn streams_to_stream_delay(s: &StreamSettings, ingest_key: &str) -> bool {
    let key = s.settings.get("key").and_then(Value::as_str).unwrap_or("");
    s.service_type == "rtmp_custom"
        && s.settings
            .get("server")
            .and_then(Value::as_str)
            .and_then(|u| RtmpUrl::parse(u).ok())
            .is_some_and(|u| u.app == "live")
        && (key == LOCAL_KEY || key == ingest_key)
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
    let obs = backup.obs.clone().unwrap_or_default();
    match bound::save(secrets, secret::OBS_BACKUP, &obs, &settings) {
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

/// The backed-up settings of the OBS `obs`, including the stream key. Refused if
/// the saved ones belong to another OBS.
fn backup_settings(
    secrets: &dyn SecretStore,
    backup: &ObsBackup,
    obs: &str,
) -> Result<Value, ApiError> {
    // Saved by an older version, on its own: from the OBS the backup names.
    let legacy_obs = backup.obs.clone().unwrap_or_else(|| obs.to_string());
    let (from, mut settings, key) = match (saved_backup(secrets), &backup.settings_json) {
        (Some(saved), _) => (saved.owner.unwrap_or(legacy_obs), saved.value, None),
        // Not moved yet: the secret store was unavailable at startup.
        (None, Some(json)) => (
            legacy_obs,
            serde_json::from_str(json)
                .map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
            secrets.get(secret::OBS_BACKUP_KEY),
        ),
        (None, None) => {
            return Err(ApiError(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the saved OBS settings are missing from the secret store".into(),
            ));
        }
    };
    // They hold a stream key: they only go back to the OBS they came from.
    if from != obs {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!(
                "the saved settings are from the OBS at {from}; connect to that OBS to restore them"
            ),
        ));
    }
    if let (Some(obj), Some(key)) = (settings.as_object_mut(), key) {
        obj.insert("key".into(), key.into());
    }
    Ok(settings)
}

/// Secret values in the saved OBS settings, for redaction.
pub(crate) fn backup_secrets(secrets: &dyn SecretStore) -> Vec<String> {
    let Some(Saved {
        value: settings, ..
    }) = saved_backup(secrets)
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
    let config = st.config();
    let mut s = ObsStatus {
        has_backup: config.obs.backup.is_some(),
        password_saved: password_for(st.shared.secrets.as_ref(), &config).is_some(),
        ..Default::default()
    };
    let server = server_for_obs(st, &config).await.ok();
    let result = match obs {
        Ok(obs) => obs.info().await,
        Err(e) => Err(e),
    };
    match result {
        Ok(info) => {
            s.reachable = true;
            s.version = Some(info.version);
            s.streaming = info.streaming;
            s.configured = server.is_some_and(|server| info.stream.points_to(&server));
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
    let _wizard = st.shared.obs_lock.lock().await;
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
    // Connected: remember the settings, and the password with them (if they
    // cannot be saved, the one saved before stays).
    let id = obs_id(&host, body.port);
    st.change_settings_and_secrets(
        &[secret::OBS_PASSWORD],
        |secrets| {
            if !body.password.is_empty() {
                bound::save(secrets, secret::OBS_PASSWORD, &id, &body.password)
            } else if !same_obs {
                // The saved password belonged to the previous OBS.
                secrets.delete(secret::OBS_PASSWORD)
            } else {
                Ok(())
            }
        },
        KeyChange::Keep,
        |c| {
            c.obs.host = host.clone();
            c.obs.port = body.port;
        },
    )?;
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
fn change(st: &AppState, f: impl FnOnce(&mut Config)) -> Result<Config, ApiError> {
    let (config, ()) = st.change_settings(KeyChange::Keep, f)?;
    Ok(config)
}

async fn configure(
    State(st): State<AppState>,
    Json(body): Json<ConfigureBody>,
) -> Result<Json<ConfigureResult>, ApiError> {
    // One wizard step at a time: the OBS in the settings stays the one this step
    // talks to (settings changes from the dashboard do not touch it).
    let _wizard = st.shared.obs_lock.lock().await;
    let config = st.config();
    // Both addresses first: nothing changes unless OBS can reach them.
    let server = server_for_obs(&st, &config).await?;
    let overlay = match body.add_overlay {
        true => Some(overlay_for_obs(&st, &config).await?),
        false => None,
    };
    let obs = Obs::connect(&target(&st, &config)).await?;
    let info = obs.info().await?;
    if info.streaming {
        return Err(ObsError::Streaming.into());
    }
    let links = urls(&st);
    let mut messages = Vec::new();
    let mut imported_key = false;

    if info.stream.points_to(&server) {
        messages.push("OBS already streams through stream-delay.".to_string());
    } else if streams_to_stream_delay(&info.stream, &links.obs_key) {
        // An earlier address of stream-delay: keep the backup of OBS's own settings.
        obs.stream_to(&server, &links.obs_key).await?;
        info!("OBS now streams to {server} (it streamed to an earlier address)");
        messages.push("OBS now streams to stream-delay's current address.".to_string());
    } else {
        // Back up OBS's settings. They hold the stream key and maybe a server
        // password, so they are a secret, not part of the config file.
        let id = obs_id(&config.obs.host, config.obs.port);
        let backup = ObsBackup {
            service_type: info.stream.service_type.clone(),
            obs: Some(id.clone()),
            settings_json: None,
        };

        // A Twitch key, for Twitch (the destination is set to it below).
        let key = match info.stream.twitch_key() {
            Some(key) if body.import_key => {
                imported_key = true;
                messages.push("Your Twitch stream key was moved into stream-delay.".to_string());
                KeyChange::Save {
                    server: SERVICES[0].url.into(),
                    key: key.into(),
                }
            }
            _ => KeyChange::Keep,
        };
        // Saved with the settings that name it, or not at all.
        st.change_settings_and_secrets(
            &[secret::OBS_BACKUP, secret::OBS_BACKUP_KEY],
            |secrets| {
                bound::save(secrets, secret::OBS_BACKUP, &id, &info.stream.settings)?;
                let _ = secrets.delete(secret::OBS_BACKUP_KEY);
                Ok(())
            },
            key,
            |c| {
                c.obs.backup = Some(backup.clone());
                if imported_key {
                    c.destination.key_mode = KeyMode::Stored;
                    // A Twitch key only goes to Twitch.
                    if !same_server(SERVICES[0].url, &c.destination.url) {
                        c.destination.service = "twitch".into();
                        c.destination.url = SERVICES[0].url.into();
                    }
                }
            },
        )?;

        obs.stream_to(&server, &links.obs_key).await?;
        info!("OBS now streams to {server}");
        messages.push("OBS now streams through stream-delay.".to_string());
    }

    let mut overlay_added = false;
    if let Some(overlay) = overlay {
        let change = obs.add_browser_source(OVERLAY_SOURCE, &overlay).await?;
        overlay_added = change == SourceChange::Added;
        messages.push(match change {
            SourceChange::Added => {
                format!("Added the \"{OVERLAY_SOURCE}\" browser source to your current scene.")
            }
            SourceChange::Updated => {
                format!("Updated the address of the \"{OVERLAY_SOURCE}\" source.")
            }
            SourceChange::Unchanged => format!("The \"{OVERLAY_SOURCE}\" source already exists."),
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
    let _wizard = st.shared.obs_lock.lock().await;
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
    let settings = backup_settings(
        st.shared.secrets.as_ref(),
        &backup,
        &obs_id(&config.obs.host, config.obs.port),
    )?;
    let obs = Obs::connect(&target(&st, &config)).await?;
    obs.restore(&StreamSettings {
        service_type: backup.service_type,
        settings,
    })
    .await?;
    // The settings first: if they cannot be saved, the backup stays, and
    // restoring again is safe.
    change(&st, |c| c.obs.backup = None).map_err(|e| {
        ApiError(
            e.0,
            format!(
                "OBS's own stream settings are back, but stream-delay could not save its \
                 settings, so it keeps the backup; try again: {}",
                e.1
            ),
        )
    })?;
    // Nothing names the backup any more.
    for name in [secret::OBS_BACKUP, secret::OBS_BACKUP_KEY] {
        if let Err(e) = st.shared.secrets.delete(name) {
            warn!("could not delete the saved {name}: {e}");
        }
    }
    info!("restored OBS's original stream settings");
    Ok(Json(build_status(&st, Ok(obs)).await))
}

#[cfg(test)]
mod tests {
    use streamdelay_config::MemorySecrets;

    use super::*;

    #[tokio::test]
    async fn an_obs_at_this_computers_own_address_is_local() {
        for host in ["localhost", "127.0.0.1", "[::1]", " 127.0.0.2 "] {
            assert!(is_local(host).await, "{host}");
        }
        // A documentation address no computer has.
        assert!(!is_local("192.0.2.1").await);
        // This computer's LAN address, if it has a route to one.
        if let Some(own) = local_ip_towards("192.0.2.1", 4455).await {
            assert!(is_local(&own.to_string()).await, "{own}");
        }
    }

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
        let settings = backup_settings(&secrets, &backup, "127.0.0.1:4456").unwrap();
        assert_eq!(settings["key"], "live_1_abc");
        assert_eq!(settings["password"], "pw");
        assert_eq!(backup_secrets(&secrets), vec!["live_1_abc", "pw"]);
        assert!(!migrate_backup(&mut config, &secrets), "only once");
        // Saved with the OBS they came from, and only restored to it.
        let e = backup_settings(&secrets, &backup, "127.0.0.1:4455").unwrap_err();
        assert_eq!(e.0, StatusCode::CONFLICT);
    }

    #[test]
    fn a_password_is_only_used_with_its_obs() {
        let secrets = MemorySecrets::default();
        let mut config = Config::default();
        config.obs.port = 4455;
        // Saved by an older version: belongs to the OBS in the settings.
        secrets.set(secret::OBS_PASSWORD, "legacy").unwrap();
        assert_eq!(password_for(&secrets, &config).as_deref(), Some("legacy"));
        bound::save(&secrets, secret::OBS_PASSWORD, "127.0.0.1:4456", &"pw-b").unwrap();
        assert_eq!(password_for(&secrets, &config), None);
        config.obs.port = 4456;
        assert_eq!(password_for(&secrets, &config).as_deref(), Some("pw-b"));
        assert_eq!(saved_password(&secrets).as_deref(), Some("pw-b"));
    }

    #[tokio::test]
    async fn the_overlay_address_is_one_obs_can_load() {
        let state = |edit: fn(&mut Config)| async move {
            let relay = streamdelay_relay::start(streamdelay_relay::RelayConfig {
                ingest_bind: "127.0.0.1:0".parse().unwrap(),
                ..Default::default()
            })
            .await
            .unwrap();
            let mut config = Config::default();
            config.api.token = "0123456789abcdef".into();
            edit(&mut config);
            let st = crate::state(
                relay,
                config.clone(),
                std::sync::Arc::new(MemorySecrets::default()),
                7788,
                None,
            );
            (st, config)
        };
        let read = |st: &AppState| st.shared.tokens.get(Scope::Read).to_string();

        // OBS on this computer: the usual link.
        let (st, c) = state(|_| {}).await;
        assert_eq!(overlay_for_obs(&st, &c).await.unwrap(), urls(&st).overlay);

        // On another computer, which only pages served to the network reach.
        let (st, c) = state(|c| c.obs.host = "192.0.2.10".into()).await;
        let e = overlay_for_obs(&st, &c).await.unwrap_err();
        assert_eq!(e.0, StatusCode::CONFLICT);
        assert!(e.1.contains("--allow-lan"), "{}", e.1);
        let (st, c) = state(|c| {
            c.obs.host = "192.0.2.10".into();
            c.api.bind = "0.0.0.0:7788".parse().unwrap();
        })
        .await;
        assert_eq!(
            overlay_for_obs(&st, &c).await.unwrap_err().0,
            StatusCode::CONFLICT
        );
        let (st, c) = state(|c| {
            c.obs.host = "192.0.2.10".into();
            c.api.bind = "192.0.2.5:7788".parse().unwrap();
            c.api.allow_lan = true;
        })
        .await;
        assert_eq!(
            overlay_for_obs(&st, &c).await.unwrap(),
            format!("http://192.0.2.5:7788/overlay?token={}", read(&st))
        );
        // Listening everywhere: the address OBS's computer reaches this one at.
        let (st, c) = state(|c| {
            c.obs.host = "192.0.2.10".into();
            c.api.bind = "0.0.0.0:7788".parse().unwrap();
            c.api.allow_lan = true;
        })
        .await;
        if let Some(own) = local_ip_towards("192.0.2.10", 4455).await {
            assert_eq!(
                overlay_for_obs(&st, &c).await.unwrap(),
                format!("http://{own}:7788/overlay?token={}", read(&st))
            );
        }
    }

    #[test]
    fn obs_ids_compare_hosts_loosely() {
        assert_eq!(obs_id(" LocalHost ", 4455), obs_id("localhost", 4455));
        assert_ne!(obs_id("127.0.0.1", 4455), obs_id("127.0.0.1", 4456));
        assert_eq!(obs_id("::1", 4455), "[::1]:4455");
        assert_eq!(obs_id("[::1]", 4455), "[::1]:4455");
    }
}
