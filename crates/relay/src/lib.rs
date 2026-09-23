//! The stream-delay relay: accepts a stream from OBS, runs it through the delay
//! engine and publishes it to the destination.
//!
//! ```text
//! OBS ──RTMP──▶ ingest ──▶ core task (Engine) ──▶ egress ──RTMP(S)──▶ Twitch
//!                               ▲
//!                         RelayHandle (commands, state)
//! ```

mod core;
mod egress;
mod ingest;
mod io;

use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

use serde::Serialize;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};

pub use streamdelay_engine::{
    Ack, Command, DelayMode, EngineConfig, EngineError, GoLiveWhen, Phase, Snapshot,
};
pub use streamdelay_rtmp::RtmpUrl;

/// Where to publish the delayed stream.
#[derive(Clone, PartialEq, Eq)]
pub struct Destination {
    /// For example `rtmp://live.twitch.tv/app`.
    pub url: String,
    pub key: DestinationKey,
}

#[derive(Clone, PartialEq, Eq)]
pub enum DestinationKey {
    /// Use this stream key.
    Fixed(String),
    /// Forward whatever key the encoder publishes with.
    Passthrough,
}

impl fmt::Debug for Destination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let url = RtmpUrl::parse(&self.url)
            .map(|u| u.redacted())
            .unwrap_or_default();
        let key = match self.key {
            DestinationKey::Fixed(_) => "fixed",
            DestinationKey::Passthrough => "passthrough",
        };
        f.debug_struct("Destination")
            .field("url", &url)
            .field("key", &key)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// Address OBS connects to. Defaults to `127.0.0.1:1935`.
    pub ingest_bind: SocketAddr,
    /// If set, the encoder must publish to this application name.
    pub ingest_app: Option<String>,
    /// If set, the encoder must publish with this stream key. Required when
    /// `ingest_bind` is not a loopback address.
    pub ingest_key: Option<String>,
    pub destination: Option<Destination>,
    pub engine: EngineConfig,
    /// How long to keep the destination connected after the encoder disconnects, so a
    /// quick encoder reconnect continues the same broadcast.
    pub encoder_grace: Duration,
    /// An encoder connection that has not started publishing within this time of
    /// connecting (handshake included) is closed.
    pub publish_timeout: Duration,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            ingest_bind: SocketAddr::from(([127, 0, 0, 1], 1935)),
            ingest_app: None,
            ingest_key: None,
            destination: None,
            engine: EngineConfig::default(),
            encoder_grace: Duration::from_secs(30),
            publish_timeout: Duration::from_secs(15),
        }
    }
}

#[derive(Debug, Error)]
pub enum RelayError {
    #[error("could not listen on {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        source: std::io::Error,
    },
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error("invalid destination URL: {0}")]
    Url(#[from] streamdelay_rtmp::url::UrlError),
    #[error(
        "the RTMP input on {0} can be reached from other devices, so it needs an ingest \
         key (--ingest-key or STREAMDELAY_INGEST_KEY); otherwise anyone who can reach it \
         could stream to your channel"
    )]
    IngestKeyRequired(SocketAddr),
    #[error("the relay has shut down")]
    Closed,
}

/// Connection state of the destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum EgressStatus {
    /// No destination configured.
    #[default]
    Disabled,
    /// Waiting for a stream to send.
    Idle,
    Connecting,
    Live,
    /// The connection failed and will be retried.
    Retrying,
}

#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct EgressState {
    pub status: EgressStatus,
    /// Destination URL without the stream key.
    pub destination: Option<String>,
    pub last_error: Option<String>,
    pub bitrate_kbps: u64,
    /// Bytes queued for the destination but not yet written (upload bottleneck).
    pub backlog_bytes: u64,
    pub reconnects: u64,
}

#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct IngestState {
    pub listen: String,
    pub connected: bool,
    pub peer: Option<String>,
    pub app: Option<String>,
    pub last_error: Option<String>,
}

/// Everything the UI needs, published a few times per second.
#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct RelayState {
    pub delay: Snapshot,
    pub ingest: IngestState,
    pub egress: EgressState,
}

pub(crate) enum Control {
    Command(Command, oneshot::Sender<Result<Ack, EngineError>>),
    SetDestination(Option<Destination>),
    Shutdown(oneshot::Sender<()>),
}

/// Handle to a running relay. Cheap to clone.
#[derive(Clone)]
pub struct RelayHandle {
    control: mpsc::UnboundedSender<Control>,
    state: watch::Receiver<RelayState>,
    ingest_addr: SocketAddr,
}

impl RelayHandle {
    /// The address the ingest server is listening on.
    pub fn ingest_addr(&self) -> SocketAddr {
        self.ingest_addr
    }

    pub async fn command(&self, cmd: Command) -> Result<Ack, RelayError> {
        let (tx, rx) = oneshot::channel();
        self.control
            .send(Control::Command(cmd, tx))
            .map_err(|_| RelayError::Closed)?;
        Ok(rx.await.map_err(|_| RelayError::Closed)??)
    }

    pub async fn set_delay(&self, ms: u64, mode: DelayMode) -> Result<Ack, RelayError> {
        self.command(Command::SetDelay { ms, mode }).await
    }

    pub async fn go_live(&self, when: GoLiveWhen) -> Result<Ack, RelayError> {
        self.command(Command::GoLive(when)).await
    }

    /// Changes the destination; takes effect on the next connection.
    pub fn set_destination(&self, dest: Option<Destination>) -> Result<(), RelayError> {
        if let Some(d) = &dest {
            RtmpUrl::parse(&d.url)?;
        }
        self.control
            .send(Control::SetDestination(dest))
            .map_err(|_| RelayError::Closed)
    }

    /// Latest state.
    pub fn state(&self) -> RelayState {
        self.state.borrow().clone()
    }

    /// A receiver that is notified whenever the state changes.
    pub fn subscribe(&self) -> watch::Receiver<RelayState> {
        self.state.clone()
    }

    /// Stops the relay, cleanly unpublishing from the destination.
    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self.control.send(Control::Shutdown(tx)).is_ok() {
            let _ = tokio::time::timeout(Duration::from_secs(3), rx).await;
        }
    }
}

/// Starts the relay on the current tokio runtime.
pub async fn start(mut config: RelayConfig) -> Result<RelayHandle, RelayError> {
    if let Some(d) = &config.destination {
        RtmpUrl::parse(&d.url)?;
    }
    config.ingest_key = config.ingest_key.filter(|k| !k.is_empty());
    if config.ingest_key.is_none() && !config.ingest_bind.ip().to_canonical().is_loopback() {
        return Err(RelayError::IngestKeyRequired(config.ingest_bind));
    }
    let listener = TcpListener::bind(config.ingest_bind)
        .await
        .map_err(|source| RelayError::Bind {
            addr: config.ingest_bind,
            source,
        })?;
    let ingest_addr = listener.local_addr().map_err(|source| RelayError::Bind {
        addr: config.ingest_bind,
        source,
    })?;
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let initial = RelayState {
        ingest: IngestState {
            listen: ingest_addr.to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    let (state_tx, state_rx) = watch::channel(initial);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(ingest::listen(
        listener,
        config.publish_timeout,
        events_tx.clone(),
        shutdown_rx,
    ));
    tokio::spawn(core::run(
        config,
        ingest_addr,
        control_rx,
        events_tx,
        events_rx,
        state_tx,
        shutdown_tx,
    ));
    Ok(RelayHandle {
        control: control_tx,
        state: state_rx,
        ingest_addr,
    })
}
