//! Application wiring shared by `streamdelayd` and the desktop app: load settings,
//! start the relay and the control server, and apply settings changes.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::Serialize;
use streamdelay_config::DelayConfig;
use streamdelay_config::{Config, ConfigError, KeyMode, SecretStore, secret};
use streamdelay_relay::{
    Ack, DelayMode, Destination, DestinationKey, EngineConfig, GoLiveWhen, RelayConfig, RelayError,
    RelayHandle, RtmpUrl,
};
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::auth::{Scope, Tokens};
use crate::{AppState, Shared, routes};

/// Stream key OBS uses towards stream-delay when no ingest key is set (any value
/// works then; this one is recognizable).
pub(crate) const LOCAL_KEY: &str = "streamdelay";

/// Command-line or environment settings that override the config file for this run.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub ingest: Option<SocketAddr>,
    pub api: Option<SocketAddr>,
    pub destination_url: Option<String>,
    /// Destination key for this run only (never written to disk).
    pub destination_key: Option<String>,
    pub passthrough: bool,
    pub token: Option<String>,
    pub max_delay_seconds: Option<u64>,
    pub start_delay_seconds: Option<f64>,
    pub grace_seconds: Option<u64>,
    pub allow_lan: bool,
    pub ingest_key: Option<String>,
}

pub struct AppOptions {
    /// Config file to load and save. `None` keeps settings in memory.
    pub config_path: Option<PathBuf>,
    pub secrets: Arc<dyn SecretStore>,
    pub overrides: Overrides,
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Relay(#[from] RelayError),
    #[error("could not start the control server on {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        source: std::io::Error,
    },
    #[error(
        "stream-delay is already running, for example as streamdelayd in a terminal \
         window. Close it, then start the app again."
    )]
    AlreadyRunning,
    #[error("invalid settings: {0}")]
    Settings(String),
    #[error(
        "the API token must be at least {MIN_TOKEN_LEN} characters long; leave it unset \
         to use a generated one"
    )]
    WeakToken,
    #[error(
        "the API token may only contain letters, digits and . _ ~ - (it goes into links); \
         leave it unset to use a generated one"
    )]
    TokenCharacters,
}

/// Shortest API token accepted. Generated tokens have 32 characters; a short one
/// chosen by hand could be guessed, as nothing slows down wrong tokens.
pub const MIN_TOKEN_LEN: usize = 16;

/// Links for the streamer to paste into OBS.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Urls {
    /// Full control, including settings and stream keys.
    pub dashboard: String,
    /// Can change the delay, nothing else.
    pub dock: String,
    /// Can only read the state.
    pub overlay: String,
    /// Value for OBS Settings → Stream → Server.
    pub obs_server: String,
    /// Value for OBS Settings → Stream → Stream Key (the ingest key, if one is set).
    pub obs_key: String,
}

/// A running stream-delay instance.
pub struct App {
    state: AppState,
    pub api_addr: SocketAddr,
}

impl App {
    pub async fn start(opts: AppOptions) -> Result<App, AppError> {
        let mut config = match &opts.config_path {
            Some(p) => Config::load_or_create(p)?,
            None => {
                let mut c = Config::default();
                c.api.token = streamdelay_config::new_token();
                c
            }
        };
        // Older versions (or hand edits) may have the stream key in the destination
        // URL; keep it in the secret store instead, like a key entered on its own.
        if let (url, Some(key)) = split_url_key(&config.destination.url) {
            let stored = opts.secrets.get(secret::DESTINATION_KEY);
            let moved = if stored.is_some_and(|k| !k.is_empty()) {
                // A stored key already took precedence over the one in the URL.
                Ok(())
            } else {
                crate::dest_key::save(opts.secrets.as_ref(), &url, &key)
            };
            match moved {
                Ok(()) => {
                    config.destination.url = url;
                    if let Some(p) = &opts.config_path {
                        config.save(p)?;
                    }
                    info!("moved the stream key out of the destination URL");
                }
                Err(e) => warn!("could not move the stream key out of the destination URL: {e}"),
            }
        }
        if crate::obs_routes::migrate_backup(&mut config, opts.secrets.as_ref())
            && let Some(p) = &opts.config_path
        {
            config.save(p)?;
        }
        // Older versions saved the key on its own; it belongs to the destination
        // it was used with, and from now on only goes there.
        match crate::dest_key::bind_legacy(opts.secrets.as_ref(), &config.destination.url) {
            Ok(true) => info!("the saved stream key is now tied to its destination server"),
            Ok(false) => {}
            // It stays tied to the destination in the settings file, as before.
            Err(e) => warn!("could not tie the saved stream key to its destination: {e}"),
        }
        // As saved, without this run's overrides.
        let mut saved = config.clone();

        let o = &opts.overrides;
        let mut key_override = o.destination_key.clone();
        if let Some(v) = o.ingest {
            config.ingest.bind = v;
        }
        if let Some(v) = o.api {
            config.api.bind = v;
        }
        if let Some(v) = &o.destination_url {
            let (url, key) = split_url_key(v);
            config.destination.url = url;
            config.destination.service = "custom".into();
            key_override = key_override.or(key);
        }
        if o.passthrough {
            config.destination.key_mode = KeyMode::Passthrough;
        }
        if let Some(v) = &o.token {
            config.api.token = v.clone();
        }
        if let Some(v) = o.max_delay_seconds {
            config.delay.max_seconds = v;
        }
        if let Some(v) = o.start_delay_seconds {
            config.delay.start_seconds = v;
        }
        if let Some(v) = o.grace_seconds {
            config.ingest.grace_seconds = v;
        }
        if o.allow_lan {
            config.api.allow_lan = true;
        }
        if let Some(k) = &o.ingest_key {
            config.ingest.key = Some(k.clone());
        }
        // Reachable from other devices: without a key anyone who can connect could
        // stream to the destination. Generate one and keep it for the next run.
        if config.ingest.key.as_deref().is_none_or(str::is_empty)
            && !config.ingest.bind.ip().to_canonical().is_loopback()
        {
            let key = streamdelay_config::new_token();
            saved.ingest.key = Some(key.clone());
            if let Some(p) = &opts.config_path {
                saved.save(p)?;
            }
            warn!(
                "the RTMP input on {} can be reached from other devices, so encoders must \
                 use the stream key shown with the OBS server address",
                config.ingest.bind
            );
            config.ingest.key = Some(key);
        }
        crate::settings::validate_limits(&config.delay, config.ingest.grace_seconds)
            .map_err(AppError::Settings)?;
        // Presets longer than the maximum (after a lower --max-delay, or a hand
        // edit) could only fail, and would keep the dashboard from saving the
        // delay settings.
        let max = config.delay.max_seconds as f64;
        let presets = config.delay.presets.len();
        config.delay.presets.retain(|p| p.seconds <= max);
        if config.delay.presets.len() < presets {
            warn!("ignoring delay presets longer than the maximum delay of {max} s");
        }
        if config.delay.presets.is_empty() {
            config.delay.presets.push(streamdelay_config::Preset {
                seconds: 0.0,
                mode: config.delay.default_mode,
            });
        }
        if config.api.token.chars().count() < MIN_TOKEN_LEN {
            return Err(AppError::WeakToken);
        }
        // Links and the WebSocket carry it in a query string unencoded.
        if !config
            .api
            .token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._~-".contains(&b))
        {
            return Err(AppError::TokenCharacters);
        }
        if config.destination.key_mode == KeyMode::Passthrough
            && config.ingest.key.as_deref().is_some_and(|k| !k.is_empty())
        {
            warn!(
                "passthrough forwards the key OBS streams with, which has to be the ingest \
                 key here; store the destination stream key instead"
            );
        }

        // Bind the API first: if its port is taken nothing else has started yet, so
        // the caller can retry with another port.
        let listener =
            TcpListener::bind(config.api.bind)
                .await
                .map_err(|source| AppError::Bind {
                    addr: config.api.bind,
                    source,
                })?;
        let api_addr = listener.local_addr().map_err(|source| AppError::Bind {
            addr: config.api.bind,
            source,
        })?;
        let key_override_url = config.destination.url.clone();
        // The stored key belongs to the destination in the settings file; a
        // destination given on the command line for another server does not get it.
        let key = key_override.clone().or_else(|| {
            crate::dest_key::for_url(
                opts.secrets.as_ref(),
                &config.destination.url,
                &saved.destination.url,
            )
        });
        let relay = streamdelay_relay::start(RelayConfig {
            ingest_bind: config.ingest.bind,
            destination: destination(&config, key),
            engine: engine_config(&config),
            encoder_grace: Duration::from_secs(config.ingest.grace_seconds),
            ingest_key: config.ingest.key.clone().filter(|k| !k.is_empty()),
            ..Default::default()
        })
        .await?;
        if config.delay.start_seconds > 0.0 {
            let ms = (config.delay.start_seconds * 1000.0) as u64;
            if let Err(e) = relay.set_delay(ms, config.delay.default_mode).await {
                warn!("could not apply the start delay: {e}");
            }
        }

        let (config_tx, _) = watch::channel(config.clone());
        let state = AppState {
            shared: Arc::new(Shared {
                relay,
                tokens: Tokens::new(&config.api.token),
                allow_lan: config.api.allow_lan,
                allowed_hosts: config.api.allowed_hosts.clone(),
                download_codes: Default::default(),
                saved: std::sync::Mutex::new(saved),
                config: RwLock::new(config),
                config_path: opts.config_path,
                save_lock: std::sync::Mutex::new(()),
                secrets: opts.secrets,
                key_override,
                key_override_url,
                config_tx,
                port: api_addr.port(),
                restart_required: AtomicBool::new(false),
                overlays: watch::channel(0).0,
                update_check: RwLock::new(None),
                applied_destination: std::sync::Mutex::new(None),
                obs_lock: tokio::sync::Mutex::new(()),
            }),
        };
        // What the relay was started with.
        *state
            .shared
            .applied_destination
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            state.destination(&state.config());
        let router = routes::router(state.clone());
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, router).await {
                tracing::error!("control server stopped: {e}");
            }
        });
        let app = App { state, api_addr };
        info!(
            "stream-delay ready: OBS server {}, dashboard {}",
            app.urls().obs_server,
            app.urls().dashboard.split('?').next().unwrap_or_default()
        );
        Ok(app)
    }

    pub fn relay(&self) -> &RelayHandle {
        self.state.relay()
    }

    pub fn config(&self) -> Config {
        self.state.config()
    }

    /// Notified whenever settings change.
    pub fn subscribe_config(&self) -> watch::Receiver<Config> {
        self.state.shared.config_tx.subscribe()
    }

    pub fn urls(&self) -> Urls {
        urls(&self.state)
    }

    /// Makes the dashboard's "Check for updates" run `check` (the desktop app's
    /// updater). Without it, the dashboard links to the releases page.
    pub fn on_update_check(&self, check: impl Fn() + Send + Sync + 'static) {
        *self
            .state
            .shared
            .update_check
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(check));
    }

    /// True until a stream key (or passthrough) is configured.
    pub fn needs_setup(&self) -> bool {
        let c = self.config();
        c.destination.key_mode == KeyMode::Stored
            && self.state.key_override(&c.destination.url).is_none()
            && self.state.stored_key(&c.destination.url).is_none()
    }

    /// Applies a preset by index (used by hotkeys and the tray menu).
    pub async fn apply_preset(&self, index: usize) -> Result<(), RelayError> {
        let presets = self.config().delay.presets;
        let Some(p) = presets.get(index) else {
            return Ok(());
        };
        if p.seconds <= 0.0 {
            self.relay().go_live(GoLiveWhen::Now).await?;
        } else {
            self.relay()
                .set_delay((p.seconds * 1000.0).round() as u64, p.mode)
                .await?;
        }
        Ok(())
    }

    /// Throws away what has not aired yet, the way the settings say (used by
    /// hotkeys and the tray menu).
    pub async fn dump(&self) -> Result<Ack, RelayError> {
        let mode = dump_mode(&self.config().delay, None);
        self.relay().dump(mode).await
    }

    pub async fn shutdown(&self) {
        self.relay().shutdown().await;
    }
}

/// How a dump covers the stream: as asked, else the default mode. Without the
/// rolling buffer there is nothing to replay, so the slate covers it.
pub(crate) fn dump_mode(delay: &DelayConfig, asked: Option<DelayMode>) -> DelayMode {
    if delay.keep_buffer {
        asked.unwrap_or(delay.default_mode)
    } else {
        DelayMode::Mask
    }
}

/// Where local clients reach a listener: loopback of the same family when it
/// listens on every interface (an IPv6 socket need not accept IPv4: on Windows it
/// doesn't). Formatting a `SocketAddr` puts IPv6 addresses in brackets, as URLs
/// need.
pub fn reachable(addr: SocketAddr) -> SocketAddr {
    match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => {
            SocketAddr::from((Ipv4Addr::LOCALHOST, addr.port()))
        }
        IpAddr::V6(ip) if ip.is_unspecified() => {
            SocketAddr::from((Ipv6Addr::LOCALHOST, addr.port()))
        }
        _ => addr,
    }
}

pub(crate) fn urls(state: &AppState) -> Urls {
    let c = state.config();
    let host = reachable(SocketAddr::new(c.api.bind.ip(), state.shared.port));
    let token = |s| state.shared.tokens.get(s);
    let ingest_host = reachable(state.relay().ingest_addr());
    Urls {
        dashboard: format!("http://{host}/?token={}", token(Scope::Admin)),
        dock: format!("http://{host}/dock?token={}", token(Scope::Control)),
        overlay: format!("http://{host}/overlay?token={}", token(Scope::Read)),
        obs_server: format!("rtmp://{ingest_host}/live"),
        obs_key: c
            .ingest
            .key
            .filter(|k| !k.is_empty())
            .unwrap_or_else(|| LOCAL_KEY.into()),
    }
}

/// A destination URL as it may be shown: without a stream key, and with the
/// values of its query hidden (see [`RtmpUrl::redacted`]). Invalid URLs are
/// returned unchanged.
pub(crate) fn shown_url(url: &str) -> String {
    RtmpUrl::parse(url).map_or_else(|_| url.to_string(), |u| u.redacted())
}

/// Splits a stream key embedded in a destination URL (`rtmp://host/app/<key>`)
/// from it. URLs without a key, and invalid ones, are returned unchanged.
pub(crate) fn split_url_key(url: &str) -> (String, Option<String>) {
    match RtmpUrl::parse(url) {
        Ok(u) if u.stream_key.is_some() => (u.tc_url, u.stream_key),
        _ => (url.to_string(), None),
    }
}

/// Services whose stream keys work on several ingest hosts (RTMP and RTMPS,
/// regional servers).
const SERVICE_DOMAINS: &[&[&str]] = &[&["twitch.tv", "live-video.net"], &["youtube.com"]];

/// True when `new` publishes to another server than `old`, so a stream key meant
/// for `old` must not be sent to it. Clearing the destination is no change (the key
/// goes nowhere), but setting one after an empty or invalid URL is: otherwise
/// clearing it first would carry the key over to any server.
///
/// A service's own ingest servers (see `SERVICE_DOMAINS`) count as one, except
/// that RTMPS to RTMP is always another server: it would send the key
/// unencrypted, so it must be entered again. Anywhere else only the same endpoint
/// counts: scheme, host, port and application. Another port or application can be
/// another service on the same machine.
pub(crate) fn different_server(old: &str, new: &str) -> bool {
    let Ok(b) = RtmpUrl::parse(new) else {
        return false;
    };
    let Ok(a) = RtmpUrl::parse(old) else {
        return true;
    };
    let (host_a, host_b) = (a.host.to_ascii_lowercase(), b.host.to_ascii_lowercase());
    let service = |host: &str| {
        SERVICE_DOMAINS.iter().position(|domains| {
            domains
                .iter()
                .any(|d| host == *d || host.strip_suffix(d).is_some_and(|p| p.ends_with('.')))
        })
    };
    if a.encrypted() && !b.encrypted() {
        return true;
    }
    if let Some(s) = service(&host_a) {
        return service(&host_b) != Some(s);
    }
    !(a.scheme == b.scheme && host_a == host_b && a.port == b.port && a.app == b.app)
}

/// True when `url` is a valid address on the same server or service as `known`.
pub(crate) fn same_server(known: &str, url: &str) -> bool {
    RtmpUrl::parse(url).is_ok() && !different_server(known, url)
}

pub(crate) fn engine_config(c: &Config) -> EngineConfig {
    EngineConfig {
        max_delay_ms: c.delay.max_seconds * 1000,
        ram_cap_bytes: (c.delay.ram_cap_mb as usize).saturating_mul(1024 * 1024),
        keep_history: c.delay.keep_buffer,
        ..Default::default()
    }
}

/// Builds the relay destination from settings and the stream key for it.
pub(crate) fn destination(c: &Config, key: Option<String>) -> Option<Destination> {
    if c.destination.url.trim().is_empty() {
        return None;
    }
    let key = match c.destination.key_mode {
        KeyMode::Passthrough => DestinationKey::Passthrough,
        KeyMode::Stored => DestinationKey::Fixed(key.unwrap_or_default()),
    };
    Some(Destination {
        url: c.destination.url.trim().to_string(),
        key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_for_a_custom_server_stays_with_its_endpoint() {
        let key_for = "rtmps://relay.example:443/private";
        // The same endpoint, spelled differently.
        assert!(!different_server(key_for, "rtmps://Relay.Example/private"));
        for other in [
            "rtmps://relay.example:8443/private",
            "rtmps://relay.example/other",
            // Unencrypted.
            "rtmp://relay.example:443/private",
            "rtmp://relay.example/private",
            "rtmps://other.example/private",
        ] {
            assert!(different_server(key_for, other), "{other}");
        }
        // A service's own servers share its keys, over RTMP or RTMPS.
        for (a, b) in [
            (
                "rtmp://live.twitch.tv/app",
                "rtmps://ingest.global-contribute.live-video.net/app",
            ),
            (
                "rtmp://live.twitch.tv/app",
                "rtmp://fra05.contribute.live-video.net/app",
            ),
            (
                "rtmp://a.rtmp.youtube.com/live2",
                "rtmps://a.rtmps.youtube.com:443/live2",
            ),
        ] {
            assert!(!different_server(a, b), "{a} -> {b}");
        }
        // But from RTMPS to RTMP the key would travel unencrypted.
        for (a, b) in [
            (
                "rtmps://ingest.global-contribute.live-video.net/app",
                "rtmp://live.twitch.tv/app",
            ),
            (
                "rtmps://a.rtmps.youtube.com:443/live2",
                "rtmp://a.rtmp.youtube.com/live2",
            ),
        ] {
            assert!(different_server(a, b), "{a} -> {b}");
        }
        assert!(different_server(
            "rtmp://live.twitch.tv/app",
            "rtmp://a.rtmp.youtube.com/live2"
        ));
        assert!(different_server(
            "rtmp://live.twitch.tv/app",
            "rtmp://live.twitch.tv.evil.example/app"
        ));
    }

    #[test]
    fn links_use_reachable_bracketed_addresses() {
        let at = |s: &str| format!("http://{}/", reachable(s.parse().unwrap()));
        assert_eq!(at("127.0.0.1:7788"), "http://127.0.0.1:7788/");
        assert_eq!(at("0.0.0.0:7788"), "http://127.0.0.1:7788/");
        assert_eq!(at("[::1]:7788"), "http://[::1]:7788/");
        assert_eq!(at("[::]:7788"), "http://[::1]:7788/");
        assert_eq!(at("[fd00::5]:7788"), "http://[fd00::5]:7788/");
    }

    #[test]
    fn links_to_a_listener_on_every_interface_reach_it() {
        for any in ["0.0.0.0:0", "[::]:0"] {
            // A machine without IPv6 cannot listen on [::].
            let Ok(listener) = std::net::TcpListener::bind(any) else {
                continue;
            };
            let addr = reachable(listener.local_addr().unwrap());
            std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(5))
                .unwrap_or_else(|e| panic!("listening on {any}, {addr}: {e}"));
        }
    }
}
