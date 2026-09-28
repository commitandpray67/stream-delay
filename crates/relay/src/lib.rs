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
mod heap;
mod ingest;
mod io;
mod lifecycle;
mod sendq;

use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;

use serde::Serialize;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};

pub use streamdelay_engine::{
    Ack, Command, DelayMode, DumpOutcome, EngineConfig, EngineError, GoLiveWhen, Phase, Snapshot,
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
    /// A destination connection that accepts no data at all for this long is
    /// taken to be dead and replaced (the stream resumes from the buffer).
    pub stall_timeout: Duration,
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
            // Congestion slows the upload down, but never stops it this long.
            stall_timeout: Duration::from_secs(20),
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
    #[error(
        "the RTMP input on {addr} can be reached from other devices, so its ingest key must \
         be hard to guess, and it {why}; leave it unset to use a generated one"
    )]
    WeakIngestKey { addr: SocketAddr, why: KeyWeakness },
    #[error("the relay has shut down")]
    Closed,
}

/// Connection state of the destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum EgressStatus {
    /// Nothing to send to: no destination, or no stream key for it.
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
    /// Wrong stream keys in the last [`BAD_KEYS_SHOWN_FOR`], and the address
    /// the latest came from: someone may be guessing the ingest key.
    pub bad_keys_recent: u32,
    pub bad_key_from: Option<String>,
    /// Why the ingest key, which is accepted, is weaker than it should be.
    pub key_warning: Option<String>,
}

/// How far back [`IngestState::bad_keys_recent`] counts.
pub const BAD_KEYS_SHOWN_FOR: Duration = Duration::from_secs(600);

/// Everything the UI needs, published a few times per second.
#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct RelayState {
    pub delay: Snapshot,
    pub ingest: IngestState,
    pub egress: EgressState,
    /// The streamer ended the broadcast with "end stream". Nothing is sent until
    /// they resume or the encoder starts a new stream.
    pub ended: bool,
    /// "End stream" was asked for after what is buffered has aired: the broadcast
    /// ends once it has (then `ended` is set).
    pub ending: bool,
}

pub(crate) enum Control {
    Command(Command, oneshot::Sender<Result<Ack, EngineError>>),
    SetDestination(Option<Destination>),
    EndStream(oneshot::Sender<()>),
    EndAfterAir(oneshot::Sender<()>),
    Resume(oneshot::Sender<()>),
    SetKeepHistory(bool),
    SetRestoreAfterReconnect(bool),
    Shutdown(oneshot::Sender<()>),
}

/// Handle to a running relay. Cheap to clone.
#[derive(Clone)]
pub struct RelayHandle {
    control: mpsc::UnboundedSender<Control>,
    state: watch::Receiver<RelayState>,
    ingest_addr: SocketAddr,
    arena: streamdelay_rtmp::ArenaPool,
}

impl RelayHandle {
    /// The address the ingest server is listening on.
    pub fn ingest_addr(&self) -> SocketAddr {
        self.ingest_addr
    }

    /// Memory held by the blocks received messages are kept in (see
    /// [`streamdelay_rtmp::ArenaPool`]): at most the buffer's RAM cap plus the
    /// ingest queue budget.
    pub fn ingest_block_bytes(&self) -> usize {
        self.arena.bytes_in_use()
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

    /// Ends the broadcast now and throws away everything buffered, so none of it
    /// ever airs. The encoder may keep sending; nothing goes out until
    /// [`RelayHandle::resume`] or a new encoder session.
    pub async fn end_stream(&self) -> Result<(), RelayError> {
        let (tx, rx) = oneshot::channel();
        self.control
            .send(Control::EndStream(tx))
            .map_err(|_| RelayError::Closed)?;
        rx.await.map_err(|_| RelayError::Closed)
    }

    /// Ends the broadcast once what has been sent to stream-delay so far has
    /// aired. Nothing sent after this airs; the stream then counts as ended, as
    /// after [`RelayHandle::end_stream`].
    pub async fn end_stream_after_air(&self) -> Result<(), RelayError> {
        let (tx, rx) = oneshot::channel();
        self.control
            .send(Control::EndAfterAir(tx))
            .map_err(|_| RelayError::Closed)?;
        rx.await.map_err(|_| RelayError::Closed)
    }

    /// Throws away what has not aired yet and keeps the broadcast going with the
    /// same delay (see [`Command::Dump`]); `cover`: an overlay shows the slate.
    /// While a broadcast is ending, it ends at once instead. [`Ack::dump`] says
    /// what viewers see.
    pub async fn dump(&self, mode: DelayMode, cover: bool) -> Result<Ack, RelayError> {
        self.command(Command::Dump { mode, cover }).await
    }

    /// An overlay page painted the slate for `change` (see
    /// [`Command::SlateShown`]).
    pub async fn slate_shown(&self, change: u64) -> Result<Ack, RelayError> {
        self.command(Command::SlateShown { change }).await
    }

    /// Starts broadcasting again after [`RelayHandle::end_stream`], from content
    /// received from now on and with the current delay. Before the end has aired,
    /// cancels [`RelayHandle::end_stream_after_air`] instead.
    pub async fn resume(&self) -> Result<(), RelayError> {
        let (tx, rx) = oneshot::channel();
        self.control
            .send(Control::Resume(tx))
            .map_err(|_| RelayError::Closed)?;
        rx.await.map_err(|_| RelayError::Closed)
    }

    /// Turns the rolling buffer on or off. Off: aired content is dropped and every
    /// delay increase uses mask mode. Takes effect immediately.
    pub fn set_keep_history(&self, keep: bool) -> Result<(), RelayError> {
        self.control
            .send(Control::SetKeepHistory(keep))
            .map_err(|_| RelayError::Closed)
    }

    /// After the destination connection comes back: go back to the delay set
    /// (`true`), or resume where it left off with the delay longer by the
    /// outage. Takes effect from the next reconnect.
    pub fn set_restore_after_reconnect(&self, restore: bool) -> Result<(), RelayError> {
        self.control
            .send(Control::SetRestoreAfterReconnect(restore))
            .map_err(|_| RelayError::Closed)
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

    /// Stops the relay, cleanly unpublishing from the destination. Returns once
    /// the destination has been told the stream ended (at most a few seconds).
    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self.control.send(Control::Shutdown(tx)).is_ok() {
            let _ = tokio::time::timeout(Duration::from_secs(5), rx).await;
        }
    }
}

/// Shortest ingest key accepted when the RTMP input can be reached from other
/// devices, as for API tokens: generated keys have 32 characters.
pub const MIN_INGEST_KEY_LEN: usize = 16;

/// Estimated strength an ingest key reachable from other devices should have,
/// in bits: guessing it is then hopeless at any speed, since wrong keys are
/// only slowed down, never locked out (a correct one must always get in).
pub const STRONG_INGEST_KEY_BITS: u32 = 80;

/// How an ingest key falls short of being hard to guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyWeakness {
    /// Fewer than [`MIN_INGEST_KEY_LEN`] characters: refused.
    TooShort,
    /// A few characters over and over (`aaaa…`, `abab…`), or a run like
    /// `abcdef…`: refused.
    Pattern,
    /// About this many bits, under [`STRONG_INGEST_KEY_BITS`]: accepted (keys
    /// set before this rule keep working), with a warning.
    Guessable(u32),
}

impl KeyWeakness {
    /// Whether the relay refuses to start with it (see [`RelayError::WeakIngestKey`]).
    pub fn refused(self) -> bool {
        !matches!(self, Self::Guessable(_))
    }
}

impl std::fmt::Display for KeyWeakness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort => write!(f, "is shorter than {MIN_INGEST_KEY_LEN} characters"),
            Self::Pattern => f.write_str("is a pattern (a few characters repeated, or a run)"),
            Self::Guessable(bits) => write!(
                f,
                "has only about {bits} bits of strength (it should have {STRONG_INGEST_KEY_BITS}: \
                 more characters, or a mix of letters, digits and symbols)"
            ),
        }
    }
}

/// How `key` falls short of being hard to guess, if it does. The strength is
/// estimated from its length and the kinds of characters in it (lower case,
/// upper case, digits, others).
pub fn ingest_key_weakness(key: &str) -> Option<KeyWeakness> {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() < MIN_INGEST_KEY_LEN {
        return Some(KeyWeakness::TooShort);
    }
    let repeats = (1..=4).any(|p| chars.iter().skip(p).zip(&chars).all(|(a, b)| a == b));
    let run = |step: i64| chars.windows(2).all(|w| w[1] as i64 - w[0] as i64 == step);
    if repeats || run(1) || run(-1) {
        return Some(KeyWeakness::Pattern);
    }
    let has = |f: fn(&char) -> bool| chars.iter().any(f);
    let pool = [
        (has(char::is_ascii_lowercase), 26),
        (has(char::is_ascii_uppercase), 26),
        (has(char::is_ascii_digit), 10),
        (has(|c| !c.is_ascii_alphanumeric()), 33),
    ]
    .iter()
    .filter(|(used, _)| *used)
    .map(|(_, n)| n)
    .sum::<u32>();
    let bits = (chars.len() as f64 * f64::from(pool).log2()) as u32;
    (bits < STRONG_INGEST_KEY_BITS).then_some(KeyWeakness::Guessable(bits))
}

/// What the dashboard says about an ingest key reachable from other devices
/// that is accepted but could be stronger.
pub(crate) fn key_warning(config: &RelayConfig) -> Option<String> {
    if config.ingest_bind.ip().to_canonical().is_loopback() {
        return None;
    }
    let why = ingest_key_weakness(config.ingest_key.as_deref()?)?;
    Some(format!(
        "The ingest key {why}. Other devices can reach the RTMP input, so it should be \
         hard to guess: remove it from the settings to use a generated one."
    ))
}

/// Starts the relay on the current tokio runtime.
pub async fn start(mut config: RelayConfig) -> Result<RelayHandle, RelayError> {
    if let Some(d) = &config.destination {
        RtmpUrl::parse(&d.url)?;
    }
    config.ingest_key = config.ingest_key.filter(|k| !k.is_empty());
    if !config.ingest_bind.ip().to_canonical().is_loopback() {
        match &config.ingest_key {
            None => return Err(RelayError::IngestKeyRequired(config.ingest_bind)),
            Some(k) => match ingest_key_weakness(k) {
                Some(why) if why.refused() => {
                    return Err(RelayError::WeakIngestKey {
                        addr: config.ingest_bind,
                        why,
                    });
                }
                Some(why) => tracing::warn!(
                    "the RTMP input on {} can be reached from other devices, and its ingest \
                     key {why}; leave it unset to use a generated one",
                    config.ingest_bind
                ),
                None => {}
            },
        }
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
    let (state_tx, state_rx) = watch::channel(core::initial_state(&config, ingest_addr));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    // Blocks for what the buffer keeps and what waits for the core: whatever
    // pattern of messages a publisher sends, they cannot hold more.
    heap::keep_blocks_mapped();
    let arena = streamdelay_rtmp::ArenaPool::new(
        config
            .engine
            .ram_cap_bytes
            .saturating_add(core::INGEST_QUEUE_BUDGET),
    );
    tokio::spawn(ingest::listen(
        listener,
        config.publish_timeout,
        core::IngestTx::new(events_tx.clone(), core::INGEST_QUEUE_BUDGET, arena.clone()),
        shutdown_rx,
    ));
    tokio::spawn(core::run(
        config,
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
        arena,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ingest_keys_must_be_hard_to_guess() {
        use KeyWeakness::*;
        // Generated keys (32 hex digits), and a strong one of the shortest length.
        assert_eq!(
            ingest_key_weakness("3f9a1c7e5b2d4f60a8c1e3b5d7f90a2c"),
            None
        );
        assert_eq!(ingest_key_weakness("Xk3-9fQ2-mP7z-Lw"), None);
        // Refused.
        assert_eq!(ingest_key_weakness("choose-a-secret"), Some(TooShort));
        for pattern in [
            "aaaaaaaaaaaaaaaa",
            "abababababababab",
            "abcdabcdabcdabcd",
            "abcdefghijklmnop",
            "ponmlkjihgfedcba",
        ] {
            let w = ingest_key_weakness(pattern);
            assert!(w.is_some_and(KeyWeakness::refused), "{pattern}: {w:?}");
        }
        // Accepted with a warning: 16 lower-case letters are about 75 bits.
        assert_eq!(ingest_key_weakness("correcthorsebatt"), Some(Guessable(75)));
        assert!(!Guessable(75).refused());
        assert_eq!(
            ingest_key_weakness("correcthorsebatterys"),
            None,
            "20 of them are 94"
        );
    }
}
