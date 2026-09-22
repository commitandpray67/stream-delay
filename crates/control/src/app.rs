//! Application wiring shared by `streamdelayd` and the desktop app: load settings,
//! start the relay and the control server, and apply settings changes.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::Serialize;
use streamdelay_config::{Config, ConfigError, KeyMode, SecretStore, secret};
use streamdelay_relay::{
    Destination, DestinationKey, EngineConfig, GoLiveWhen, RelayConfig, RelayError, RelayHandle,
};
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::{AppState, Shared, routes};

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
}

/// Links for the streamer to paste into OBS.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Urls {
    pub dashboard: String,
    pub dock: String,
    pub overlay: String,
    /// Value for OBS Settings → Stream → Server.
    pub obs_server: String,
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
        let o = &opts.overrides;
        if let Some(v) = o.ingest {
            config.ingest.bind = v;
        }
        if let Some(v) = o.api {
            config.api.bind = v;
        }
        if let Some(v) = &o.destination_url {
            config.destination.url = v.clone();
            config.destination.service = "custom".into();
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
        let key_override = o.destination_key.clone();
        let relay = streamdelay_relay::start(RelayConfig {
            ingest_bind: config.ingest.bind,
            destination: destination(&config, opts.secrets.as_ref(), key_override.as_deref()),
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
                config: RwLock::new(config),
                config_path: opts.config_path,
                secrets: opts.secrets,
                key_override,
                config_tx,
                port: api_addr.port(),
                restart_required: AtomicBool::new(false),
            }),
        };
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

    /// True until a stream key (or passthrough) is configured.
    pub fn needs_setup(&self) -> bool {
        let c = self.config();
        c.destination.key_mode == KeyMode::Stored
            && self.state.shared.key_override.is_none()
            && self
                .state
                .shared
                .secrets
                .get(secret::DESTINATION_KEY)
                .is_none_or(|k| k.is_empty())
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

    pub async fn shutdown(&self) {
        self.relay().shutdown().await;
    }
}

pub(crate) fn urls(state: &AppState) -> Urls {
    let c = state.config();
    let host = if c.api.bind.ip().is_unspecified() {
        format!("127.0.0.1:{}", state.shared.port)
    } else {
        format!("{}:{}", c.api.bind.ip(), state.shared.port)
    };
    let token = &c.api.token;
    let ingest = state.relay().ingest_addr();
    let ingest_host = if ingest.ip().is_unspecified() {
        format!("127.0.0.1:{}", ingest.port())
    } else {
        ingest.to_string()
    };
    Urls {
        dashboard: format!("http://{host}/?token={token}"),
        dock: format!("http://{host}/dock?token={token}"),
        overlay: format!("http://{host}/overlay?token={token}"),
        obs_server: format!("rtmp://{ingest_host}/live"),
    }
}

pub(crate) fn engine_config(c: &Config) -> EngineConfig {
    EngineConfig {
        max_delay_ms: c.delay.max_seconds * 1000,
        ram_cap_bytes: (c.delay.ram_cap_mb as usize).saturating_mul(1024 * 1024),
        ..Default::default()
    }
}

/// Builds the relay destination from settings and secrets.
pub(crate) fn destination(
    c: &Config,
    secrets: &dyn SecretStore,
    key_override: Option<&str>,
) -> Option<Destination> {
    if c.destination.url.trim().is_empty() {
        return None;
    }
    let key = match c.destination.key_mode {
        KeyMode::Passthrough => DestinationKey::Passthrough,
        KeyMode::Stored => DestinationKey::Fixed(
            key_override
                .map(str::to_string)
                .or_else(|| secrets.get(secret::DESTINATION_KEY))
                .unwrap_or_default(),
        ),
    };
    Some(Destination {
        url: c.destination.url.trim().to_string(),
        key,
    })
}
