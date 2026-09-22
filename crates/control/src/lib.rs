//! Local control API for stream-delay.
//!
//! Security model (see `SECURITY.md`): the server binds to localhost by default and
//! every `/api` request must carry the per-install token. The `Host` header must name
//! this server (blocking DNS rebinding) and cross-origin browser requests are refused,
//! so a web page the streamer happens to visit cannot drive the delay.

mod auth;
mod routes;

use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use axum::Router;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

pub use routes::ApiError;
use streamdelay_relay::{DelayMode, RelayHandle};

#[derive(Debug, Clone)]
pub struct ControlConfig {
    pub bind: SocketAddr,
    /// Secret required on every API call.
    pub token: String,
    /// Accept requests whose Host header is not a loopback name (LAN access).
    pub allow_lan: bool,
    pub presets: Vec<Preset>,
}

/// A one-click delay setting. `seconds <= 0` means "go live now".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preset {
    pub seconds: f64,
    #[serde(default)]
    pub mode: DelayMode,
}

impl Preset {
    pub fn defaults() -> Vec<Preset> {
        [0.0, 15.0, 30.0, 60.0, 120.0]
            .into_iter()
            .map(|seconds| Preset {
                seconds,
                mode: DelayMode::Rewind,
            })
            .collect()
    }
}

/// Shared state for request handlers.
#[derive(Clone)]
pub struct AppState {
    pub relay: RelayHandle,
    pub(crate) token: Arc<str>,
    pub(crate) allow_lan: bool,
    pub(crate) port: u16,
    pub(crate) presets: Arc<RwLock<Vec<Preset>>>,
}

impl AppState {
    pub(crate) fn presets(&self) -> Vec<Preset> {
        self.presets.read().map(|p| p.clone()).unwrap_or_default()
    }
}

#[derive(Debug, Error)]
pub enum ControlError {
    #[error("could not listen on {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        source: std::io::Error,
    },
}

/// Builds the router. `port` is the port clients use in the Host header.
pub fn router(relay: RelayHandle, config: &ControlConfig, port: u16) -> Router {
    let state = AppState {
        relay,
        token: Arc::from(config.token.as_str()),
        allow_lan: config.allow_lan,
        port,
        presets: Arc::new(RwLock::new(config.presets.clone())),
    };
    routes::router(state)
}

/// A running control server.
pub struct ControlServer {
    pub addr: SocketAddr,
    pub task: JoinHandle<()>,
}

/// Binds and serves the API on the current tokio runtime.
pub async fn serve(
    relay: RelayHandle,
    config: ControlConfig,
) -> Result<ControlServer, ControlError> {
    let listener = TcpListener::bind(config.bind)
        .await
        .map_err(|source| ControlError::Bind {
            addr: config.bind,
            source,
        })?;
    let addr = listener.local_addr().map_err(|source| ControlError::Bind {
        addr: config.bind,
        source,
    })?;
    let app = router(relay, &config, addr.port());
    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("control server stopped: {e}");
        }
    });
    Ok(ControlServer { addr, task })
}

/// Generates a random 128-bit token for a new install.
pub fn generate_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
