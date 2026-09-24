//! The core task: owns the delay engine and wires ingest, egress and commands together.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use streamdelay_engine::{Command, DelayMode, Engine, EngineError, Kind, OutMsg};
use streamdelay_rtmp::amf0::Amf0Value;
use streamdelay_rtmp::{ArenaPool, RtmpUrl};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};
use tokio::time::Instant;
use tracing::{info, warn};

use crate::egress::{self, Counters, EgressCtl, Queued, Target};
use crate::lifecycle::{Effect, Facts, Lifecycle, Tick};
use crate::{
    Control, Destination, DestinationKey, EgressState, EgressStatus, IngestState, RelayConfig,
    RelayState,
};

/// Most media queued for the destination but not yet written; beyond it, output
/// stays in the delay buffer. About 10 s at 6 Mbps; normally the queue is nearly
/// empty.
const MAX_EGRESS_BACKLOG: u64 = 8 * 1024 * 1024;

/// A publisher that has sent nothing for this long is taken to be a dead
/// connection the network has not reported yet (the encoder's computer lost
/// its network): the encoder reconnecting may take its place.
const TAKEOVER_SILENCE: Duration = Duration::from_secs(2);

/// How long shutting down waits for the destination to be unpublished cleanly
/// (the egress itself gives up on a clean close after 2 s).
const SHUTDOWN_WAIT: Duration = Duration::from_secs(3);

/// Longest error text kept in the state, which every page receives. Errors can
/// carry text from the destination server.
const MAX_ERROR_LEN: usize = 300;

/// Why a publish was refused.
pub(crate) struct Rejection {
    pub reason: String,
    /// The stream key was wrong, which counts towards the address's cooldown.
    pub bad_key: bool,
}

impl Rejection {
    fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            bad_key: false,
        }
    }
}

/// Shortens error text for the state.
/// Text a client sent before proving it knows the stream key, for logs and the
/// state: anyone who can connect chooses it, so it is kept short and on one line.
pub(crate) fn untrusted(s: &str) -> String {
    const MAX: usize = 64;
    let mut out: String = s
        .chars()
        .take(MAX)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if s.chars().nth(MAX).is_some() {
        out.push('…');
    }
    out
}

fn clip(mut s: String) -> String {
    if s.len() > MAX_ERROR_LEN {
        let mut end = MAX_ERROR_LEN;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push('…');
    }
    s
}

/// Bytes of encoder media that may wait for the core at once, counting
/// [`QUEUED_OVERHEAD`] per message. A publisher sending faster than the core
/// takes it in is slowed down (its connection is not read meanwhile) instead of
/// filling memory.
pub(crate) const INGEST_QUEUE_BUDGET: usize = 32 * 1024 * 1024;

/// What a queued message costs besides its payload.
const QUEUED_OVERHEAD: usize = 128;

/// How ingest connections reach the core. Lifecycle events are sent at once;
/// media waits for room in a byte budget. Both go through the same queue, so the
/// core sees them in the order they happened.
#[derive(Clone)]
pub(crate) struct IngestTx {
    events: mpsc::UnboundedSender<Event>,
    budget: Arc<Semaphore>,
    budget_bytes: usize,
    /// Blocks every ingest connection copies messages into; see [`ArenaPool`].
    pub(crate) arena: ArenaPool,
}

impl IngestTx {
    pub(crate) fn new(
        events: mpsc::UnboundedSender<Event>,
        budget_bytes: usize,
        arena: ArenaPool,
    ) -> Self {
        Self {
            events,
            budget: Arc::new(Semaphore::new(budget_bytes)),
            budget_bytes,
            arena,
        }
    }

    /// Sends a lifecycle event (not media). Fails once the core has gone.
    pub(crate) fn send(&self, event: Event) -> Result<(), ()> {
        self.events.send(event).map_err(|_| ())
    }

    /// Room for a message of `len` bytes, waiting until the core has taken in
    /// enough of what is queued. `None` once the core has gone.
    pub(crate) async fn reserve(&self, len: usize) -> Option<OwnedSemaphorePermit> {
        let n = (len + QUEUED_OVERHEAD).min(self.budget_bytes);
        let n = u32::try_from(n).unwrap_or(u32::MAX);
        tokio::select! {
            p = self.budget.clone().acquire_many_owned(n) => p.ok(),
            () = self.events.closed() => None,
        }
    }
}

pub(crate) enum Event {
    IngestPublish {
        conn: u64,
        peer: SocketAddr,
        app: String,
        key: String,
        connect_props: Vec<(String, Amf0Value)>,
        /// Closes the connection, if another takes its place.
        close: Arc<Notify>,
        reply: oneshot::Sender<Result<(), Rejection>>,
    },
    IngestMedia {
        conn: u64,
        kind: Kind,
        ts: u64,
        payload: Bytes,
        /// Its share of the queue budget, returned once it has been handled.
        _permit: OwnedSemaphorePermit,
    },
    IngestMetadata {
        conn: u64,
        payload: Bytes,
        _permit: OwnedSemaphorePermit,
    },
    IngestClosed {
        conn: u64,
        /// Why the connection ended, if it failed.
        error: Option<String>,
        /// The encoder stopped on purpose (it unpublished), rather than losing or
        /// dropping the connection.
        unpublished: bool,
    },
    EgressConnected {
        reply: oneshot::Sender<u64>,
    },
    EgressDisconnected {
        last_written: Option<u64>,
        error: Option<String>,
    },
    EgressStatus {
        status: EgressStatus,
        error: Option<String>,
    },
}

struct Publisher {
    conn: u64,
    close: Arc<Notify>,
    /// When the encoder last sent something.
    last_seen: Instant,
}

struct Core {
    config: RelayConfig,
    engine: Engine,
    clock: Instant,
    publisher: Option<Publisher>,
    /// Key and connect properties of the most recent publisher (for passthrough).
    last_publisher: Option<(String, Vec<(String, Amf0Value)>)>,
    /// When broadcasts start and end.
    life: Lifecycle,
    egress_ctl: mpsc::UnboundedSender<EgressCtl>,
    media_tx: mpsc::UnboundedSender<Queued>,
    counters: Arc<Counters>,
    generation: u64,
    state: RelayState,
    state_tx: watch::Sender<RelayState>,
    last_written_total: u64,
    last_rate_at: Instant,
    out: Vec<OutMsg>,
}

pub(crate) async fn run(
    config: RelayConfig,
    ingest_addr: SocketAddr,
    mut control: mpsc::UnboundedReceiver<Control>,
    events_tx: mpsc::UnboundedSender<Event>,
    mut events: mpsc::UnboundedReceiver<Event>,
    state_tx: watch::Sender<RelayState>,
    // Dropped (or set) when this task ends, which stops the ingest listener and
    // closes encoder connections.
    shutdown: watch::Sender<bool>,
) {
    let (egress_ctl, egress_ctl_rx) = mpsc::unbounded_channel();
    let (media_tx, media_rx) = mpsc::unbounded_channel();
    let counters = Arc::new(Counters::default());
    tokio::spawn(egress::run(
        egress_ctl_rx,
        media_rx,
        events_tx.clone(),
        counters.clone(),
        config.stall_timeout,
    ));

    let now = Instant::now();
    let mut core = Core {
        engine: Engine::new(config.engine.clone()),
        state: RelayState {
            ingest: IngestState {
                listen: ingest_addr.to_string(),
                ..Default::default()
            },
            egress: EgressState {
                status: if config.destination.is_some() {
                    EgressStatus::Idle
                } else {
                    EgressStatus::Disabled
                },
                destination: config.destination.as_ref().map(redacted),
                ..Default::default()
            },
            ..Default::default()
        },
        life: Lifecycle::new(config.encoder_grace),
        config,
        clock: now,
        publisher: None,
        last_publisher: None,
        egress_ctl,
        media_tx,
        counters,
        generation: 0,
        state_tx,
        last_written_total: 0,
        last_rate_at: now,
        out: Vec::new(),
    };
    let mut wake: Option<u64> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let sleep_until = wake.map(|w| core.clock + Duration::from_micros(w));
        tokio::select! {
            c = control.recv() => match c {
                Some(Control::Shutdown(done)) => {
                    let _ = shutdown.send(true);
                    if core.life.egress_on() {
                        let _ = core.egress_ctl.send(EgressCtl::Stop);
                        // Wait for the clean unpublish, so the broadcast ends at once
                        // rather than when the destination notices the connection is
                        // gone.
                        let _ = tokio::time::timeout(SHUTDOWN_WAIT, async {
                            while let Some(ev) = events.recv().await {
                                match ev {
                                    // Connected just now: let it proceed to the stop.
                                    Event::EgressConnected { reply } => {
                                        let _ = reply.send(core.generation);
                                    }
                                    Event::EgressStatus {
                                        status: EgressStatus::Idle,
                                        ..
                                    } => break,
                                    _ => {}
                                }
                            }
                        })
                        .await;
                    }
                    let _ = done.send(());
                    return;
                }
                Some(c) => core.control(c),
                None => return,
            },
            Some(ev) = events.recv() => core.event(ev),
            _ = async { tokio::time::sleep_until(sleep_until.unwrap_or_else(Instant::now)).await },
                if sleep_until.is_some() => {}
            _ = tick.tick() => core.publish_state(),
        }
        wake = core.pump();
    }
}

fn redacted(d: &Destination) -> String {
    RtmpUrl::parse(&d.url)
        .map(|u| u.redacted())
        .unwrap_or_else(|_| "invalid URL".into())
}

impl Core {
    fn now(&self) -> u64 {
        self.clock.elapsed().as_micros() as u64
    }

    fn control(&mut self, c: Control) {
        let now = self.now();
        match c {
            Control::Command(cmd, reply) => {
                let cmd = match cmd {
                    Command::Dump(mode) if self.engine.can_dump() => {
                        Command::Dump(self.cut_output(mode))
                    }
                    cmd => cmd,
                };
                let r: Result<_, EngineError> = self.engine.command(now, cmd);
                if let Ok(ack) = &r {
                    info!(?cmd, ?ack, "delay command");
                }
                self.publish_state();
                let _ = reply.send(r);
            }
            Control::SetDestination(dest) => {
                self.state.egress.destination = dest.as_ref().map(redacted);
                self.state.egress.last_error = None;
                self.config.destination = dest;
                // A running connection reconnects with the new settings.
                let mut fx = Vec::new();
                self.life.destination_changed(&mut fx);
                self.apply(fx);
                self.state.egress.status = if self.config.destination.is_some() {
                    EgressStatus::Idle
                } else {
                    EgressStatus::Disabled
                };
                self.publish_state();
            }
            Control::EndStream(reply) => {
                let mut fx = Vec::new();
                self.life.end_now(&mut fx);
                self.apply(fx);
                // Publish before replying, so callers read the new state.
                self.publish_state();
                let _ = reply.send(());
            }
            Control::EndAfterAir(reply) => {
                let facts = self.facts(now);
                let mut fx = Vec::new();
                self.life.end_after_air(Instant::now(), &facts, &mut fx);
                self.apply(fx);
                self.publish_state();
                let _ = reply.send(());
            }
            Control::SetKeepHistory(keep) => {
                info!(keep, "rolling buffer setting changed");
                self.engine.set_keep_history(keep);
                self.publish_state();
            }
            Control::Resume(reply) => {
                let mut fx = Vec::new();
                self.life.resume(&mut fx);
                self.apply(fx);
                self.publish_state();
                let _ = reply.send(());
            }
            Control::Shutdown(_) => unreachable!("handled by the caller"),
        }
    }

    fn event(&mut self, ev: Event) {
        let now = self.now();
        match ev {
            Event::IngestPublish {
                conn,
                peer,
                app,
                key,
                connect_props,
                close,
                reply,
            } => {
                let result = self.accept_publisher(&app, &key);
                if result.is_ok() {
                    let old = self.publisher.take();
                    let facts = self.facts(now);
                    let mut fx = Vec::new();
                    self.life
                        .publishing(Instant::now(), old.is_some(), &facts, &mut fx);
                    // Before this stream's first message: an end mark goes after the
                    // previous one's last.
                    self.apply(fx);
                    if let Some(old) = old {
                        // The encoder reconnected while its old connection still
                        // looked open: close that one and carry on with this one.
                        info!("the encoder reconnected; closing its previous, silent connection");
                        old.close.notify_one();
                        self.engine.ingest_end(now);
                    }
                    self.engine.ingest_start(now);
                    self.last_publisher = Some((key, connect_props));
                    self.publisher = Some(Publisher {
                        conn,
                        close,
                        last_seen: Instant::now(),
                    });
                    self.state.ingest.connected = true;
                    self.state.ingest.peer = Some(peer.to_string());
                    self.state.ingest.app = Some(app);
                    self.state.ingest.last_error = None;
                } else if let Err(e) = &result {
                    self.state.ingest.last_error = Some(e.reason.clone());
                }
                let _ = reply.send(result);
                self.publish_state();
            }
            Event::IngestMedia {
                conn,
                kind,
                ts,
                payload,
                _permit: _,
            } => {
                if let Some(p) = self.publisher_mut(conn) {
                    p.last_seen = Instant::now();
                    self.engine.ingest(now, kind, ts, payload);
                }
            }
            Event::IngestMetadata {
                conn,
                payload,
                _permit: _,
            } => {
                if let Some(p) = self.publisher_mut(conn) {
                    p.last_seen = Instant::now();
                    self.engine.ingest_metadata(now, payload);
                }
            }
            Event::IngestClosed {
                conn,
                error,
                unpublished,
            } => {
                if let Some(e) = error {
                    // Shown in the dashboard and dock, so a failing encoder
                    // connection is visible without reading logs.
                    self.state.ingest.last_error =
                        Some(clip(format!("encoder connection failed: {e}")));
                    self.publish_state();
                }
                if self.is_publisher(conn) {
                    self.publisher = None;
                    self.life.encoder_left(Instant::now(), unpublished);
                    self.engine.ingest_end(now);
                    self.state.ingest.connected = false;
                    self.state.ingest.peer = None;
                    self.publish_state();
                }
            }
            Event::EgressConnected { reply } => {
                self.generation = self.engine.output_connected(now);
                let _ = reply.send(self.generation);
                self.publish_state();
            }
            Event::EgressDisconnected {
                last_written,
                error,
            } => {
                self.engine.output_disconnected(now, last_written);
                // After a stop, how the connection ended is no news (and would
                // hide why it was stopped).
                if let Some(e) = error.filter(|_| self.life.egress_on()) {
                    self.state.egress.reconnects += 1;
                    self.state.egress.last_error = Some(clip(e));
                }
                self.publish_state();
            }
            Event::EgressStatus { status, error } => {
                if !self.life.egress_on() && status != EgressStatus::Idle {
                    // Sent before the egress saw the stop: out of date.
                    return;
                }
                if self.config.destination.is_some() || status != EgressStatus::Idle {
                    self.state.egress.status = status;
                }
                // Earlier trouble is over once the destination takes the stream.
                if status == EgressStatus::Live {
                    self.state.egress.last_error = None;
                }
                if let Some(e) = error {
                    self.state.egress.last_error = Some(clip(e));
                }
                self.publish_state();
            }
        }
    }

    fn is_publisher(&self, conn: u64) -> bool {
        self.publisher.as_ref().is_some_and(|p| p.conn == conn)
    }

    fn publisher_mut(&mut self, conn: u64) -> Option<&mut Publisher> {
        self.publisher.as_mut().filter(|p| p.conn == conn)
    }

    fn accept_publisher(&self, app: &str, key: &str) -> Result<(), Rejection> {
        if let Some(want) = &self.config.ingest_app
            && want != app
        {
            return Err(Rejection::new(format!(
                "unknown application '{}' (expected '{want}')",
                untrusted(app)
            )));
        }
        // Constant time, so response timing does not reveal how much of a guess
        // was right.
        if let Some(want) = &self.config.ingest_key
            && !bool::from(subtle::ConstantTimeEq::ct_eq(
                want.as_bytes(),
                key.as_bytes(),
            ))
        {
            return Err(Rejection {
                reason: "wrong stream key for stream-delay".into(),
                bad_key: true,
            });
        }
        if matches!(
            self.config.destination,
            Some(Destination {
                key: DestinationKey::Passthrough,
                ..
            })
        ) && key.is_empty()
        {
            return Err(Rejection::new(
                "passthrough mode needs the destination stream key in the encoder",
            ));
        }
        // Checked last, so only an encoder that could publish may take over from
        // a connection that has gone silent.
        if self
            .publisher
            .as_ref()
            .is_some_and(|p| p.last_seen.elapsed() < TAKEOVER_SILENCE)
        {
            return Err(Rejection::new(
                "another encoder is already streaming to stream-delay",
            ));
        }
        Ok(())
    }

    /// What the lifecycle needs to know about the buffer and the destination.
    fn facts(&self, now: u64) -> Facts {
        Facts {
            destination: self.config.destination.is_some(),
            has_mark: self.engine.last_seq().is_some(),
            end_reached: self.engine.end_reached(),
            backlog_empty: self.counters.backlog.load(Ordering::Relaxed) == 0,
            drained: self.engine.drained(),
            has_buffered: self.engine.has_buffered(),
            output_wanted: self.engine.output_wanted(now),
            connected: self.engine.output_is_connected(),
            delay: Duration::from_micros(self.engine.effective_delay()),
        }
    }

    /// Carries out what the lifecycle asked for.
    fn apply(&mut self, fx: Vec<Effect>) {
        for e in fx {
            match e {
                Effect::StopEgress => {
                    let _ = self.egress_ctl.send(EgressCtl::Stop);
                }
                Effect::AbortEgress => {
                    let _ = self.egress_ctl.send(EgressCtl::Abort);
                }
                Effect::Discard => self.engine.discard(),
                Effect::MarkEnd => {
                    if let Some(mark) = self.engine.last_seq() {
                        self.engine.end_after(mark);
                    }
                }
                Effect::CancelEnd => self.engine.cancel_end(),
                Effect::RestartAfterEnd => self.engine.restart_after_end(),
                Effect::Explain(why) => self.state.egress.last_error = Some(why.into()),
            }
        }
    }

    /// For a dump: media handed to the destination connection but not written
    /// yet (queued, or being written) has reached no one, and never must. The
    /// connection is then reset, dropping it along with what the OS still holds
    /// from before the dump, and made again at once. What viewers saw is then
    /// unknown, so a rewind, which replays what aired, becomes a mask. Returns the
    /// mode to dump with.
    fn cut_output(&mut self, mode: DelayMode) -> DelayMode {
        // Only this task adds to it, and the egress takes off what it has
        // written only once written: zero means nothing is on its way.
        let unsent = self.counters.backlog.load(Ordering::SeqCst) > 0;
        if unsent {
            let _ = self.egress_ctl.send(EgressCtl::Cut);
        }
        if mode == DelayMode::Rewind && (unsent || !self.engine.output_is_connected()) {
            info!("the destination was behind; masking instead of rewinding");
            return DelayMode::Mask;
        }
        mode
    }

    /// Runs the engine, forwards due messages and manages the egress connection.
    fn pump(&mut self) -> Option<u64> {
        let now = self.now();
        // When the upload can't keep up, stop queueing more for the destination:
        // the rest waits in the delay buffer (which has a memory cap) instead of
        // an unbounded queue, and airs once the upload catches up.
        let queued = self.counters.backlog.load(Ordering::Relaxed);
        let room = MAX_EGRESS_BACKLOG.saturating_sub(queued);
        let wake = self.engine.poll_budget(
            now,
            &mut self.out,
            usize::try_from(room).unwrap_or(usize::MAX),
        );
        for m in self.out.drain(..) {
            self.counters
                .backlog
                .fetch_add(m.payload.len() as u64, Ordering::Relaxed);
            let _ = self.media_tx.send(Queued {
                generation: self.generation,
                msg: m,
            });
        }
        // The 250 ms state tick also re-runs this, so connect decisions never stall.
        self.manage_egress(now);
        wake
    }

    fn manage_egress(&mut self, now: u64) {
        let at = Instant::now();
        let facts = self.facts(now);
        let mut fx = Vec::new();
        let tick = self.life.tick(at, &facts, &mut fx);
        let changed = !fx.is_empty();
        self.apply(fx);
        if tick == Tick::Start
            && let Some(dest) = self.config.destination.clone()
        {
            match self.target(&dest) {
                Some(target) => {
                    self.life.started(at);
                    let _ = self.egress_ctl.send(EgressCtl::Start(target));
                }
                None => {
                    let mut fx = Vec::new();
                    self.life.start_failed(at, &mut fx);
                    self.apply(fx);
                }
            }
        }
        if changed {
            self.publish_state();
        }
    }

    fn target(&mut self, dest: &Destination) -> Option<Target> {
        let url = match RtmpUrl::parse(&dest.url) {
            Ok(u) => u,
            Err(e) => {
                warn!("invalid destination URL: {e}");
                self.state.egress.last_error = Some(format!("invalid destination URL: {e}"));
                return None;
            }
        };
        let (publisher_key, props) = self.last_publisher.clone().unwrap_or_default();
        let key = match &dest.key {
            DestinationKey::Fixed(k) if !k.is_empty() => k.clone(),
            DestinationKey::Fixed(_) => match url.stream_key.clone() {
                Some(k) => k,
                None => {
                    self.state.egress.last_error = Some("no stream key configured".into());
                    return None;
                }
            },
            DestinationKey::Passthrough => publisher_key,
        };
        Some(Target {
            url,
            key,
            connect_props: props,
        })
    }

    fn publish_state(&mut self) {
        let now = self.now();
        self.state.ended = self.life.ended();
        self.state.ending = self.life.ending();
        self.state.delay = self.engine.snapshot(now);
        let written = self.counters.written.load(Ordering::Relaxed);
        // Same 2 s window as the ingest bitrate, so the two can be compared.
        let elapsed = self.last_rate_at.elapsed().as_secs_f64();
        if elapsed >= 2.0 {
            let bits = written.saturating_sub(self.last_written_total) as f64 * 8.0;
            self.state.egress.bitrate_kbps = (bits / 1000.0 / elapsed) as u64;
            self.last_written_total = written;
            self.last_rate_at = Instant::now();
        }
        self.state.egress.backlog_bytes = self.counters.backlog.load(Ordering::Relaxed);
        if self.state.egress.status != EgressStatus::Live {
            self.state.egress.bitrate_kbps = 0;
        }
        self.state_tx.send_if_modified(|s| {
            if *s != self.state {
                *s = self.state.clone();
                true
            } else {
                false
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_from_strangers_is_kept_short_and_on_one_line() {
        assert_eq!(untrusted("live"), "live");
        assert_eq!(untrusted("a\nb\x1b[31m"), "a b [31m");
        let long = "é".repeat(60_000);
        let shown = untrusted(&long);
        assert_eq!(shown.chars().count(), 65);
        assert!(shown.ends_with('…'));
    }

    #[tokio::test]
    async fn media_waits_for_room_in_the_queue_budget() {
        let (tx, rx) = mpsc::unbounded_channel::<Event>();
        let ingest = IngestTx::new(tx, 1000, ArenaPool::unlimited());
        // Two messages of 372 bytes (500 with their overhead) fill it.
        let a = ingest.reserve(372).await.unwrap();
        let b = ingest.reserve(372).await.unwrap();
        let third = ingest.reserve(0);
        tokio::pin!(third);
        let waited = tokio::time::timeout(Duration::from_millis(50), &mut third).await;
        assert!(waited.is_err(), "a full queue must not take more");
        // The core handled one: there is room again.
        drop(a);
        let c = tokio::time::timeout(Duration::from_secs(1), third)
            .await
            .unwrap();
        assert!(c.is_some());
        drop((b, c));
        // Larger than the whole budget: it goes through, alone.
        let whole = ingest.reserve(10_000).await.unwrap();
        // The core has gone while the queue is full: waiting ends.
        drop(rx);
        assert!(ingest.reserve(10).await.is_none());
        drop(whole);
    }
}
