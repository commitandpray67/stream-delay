//! The core task: owns the delay engine and wires ingest, egress and commands together.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use streamdelay_engine::{Engine, EngineError, Kind, OutMsg};
use streamdelay_rtmp::RtmpUrl;
use streamdelay_rtmp::amf0::Amf0Value;
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::time::Instant;
use tracing::{info, warn};

use crate::egress::{self, Counters, EgressCtl, Target};
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

/// Once the encoder has gone for good, how long past the moment the last of the
/// stream was due to air a broadcast may keep draining to a destination that
/// has stopped taking data.
const DRAIN_SLACK: Duration = Duration::from_secs(30);

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
    },
    IngestMetadata {
        conn: u64,
        payload: Bytes,
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
    ingest_ended: Option<Instant>,
    /// The last encoder session ended with the encoder stopping on purpose (or
    /// there was none), so the next one is a deliberate new stream.
    encoder_stopped: bool,
    egress_ctl: mpsc::UnboundedSender<EgressCtl>,
    media_tx: mpsc::UnboundedSender<(u64, OutMsg)>,
    counters: Arc<Counters>,
    egress_running: bool,
    /// When the egress was last started.
    egress_started: Instant,
    /// The encoder started a new stream while the previous one's end was still
    /// airing: that broadcast ends at the engine's end mark, then a new one starts.
    restart_after_end: bool,
    /// When a broadcast ending at the end mark ends regardless, if the destination
    /// stops taking data meanwhile.
    end_deadline: Option<Instant>,
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
        config,
        clock: now,
        publisher: None,
        last_publisher: None,
        ingest_ended: None,
        encoder_stopped: true,
        egress_ctl,
        media_tx,
        counters,
        egress_running: false,
        egress_started: now,
        restart_after_end: false,
        end_deadline: None,
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
                    if core.egress_running {
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
                if self.egress_running {
                    // Reconnect with the new settings.
                    self.egress_running = false;
                    let _ = self.egress_ctl.send(EgressCtl::Stop);
                }
                self.state.egress.status = if self.config.destination.is_some() {
                    EgressStatus::Idle
                } else {
                    EgressStatus::Disabled
                };
                self.publish_state();
            }
            Control::EndStream(reply) => {
                // Drop the buffer first so nothing buffered can be sent, then reset
                // the destination connection, which ends the broadcast without
                // delivering what the OS still holds to send.
                self.engine.discard();
                if self.egress_running {
                    self.egress_running = false;
                    let _ = self.egress_ctl.send(EgressCtl::Abort);
                }
                self.clear_end();
                self.state.ended = true;
                info!("stream ended by the streamer; buffered content discarded");
                // Publish before replying, so callers read the new state.
                self.publish_state();
                let _ = reply.send(());
            }
            Control::EndAfterAir(reply) => {
                if !self.state.ended && !self.state.ending {
                    // Nothing is left to air, or nothing can. While connected, even
                    // with nothing buffered, the end waits for what is still queued
                    // for the destination.
                    let nothing_to_air = self.config.destination.is_none()
                        || (self.engine.drained() && !self.egress_running);
                    match self.engine.last_seq() {
                        Some(mark) if !nothing_to_air => {
                            self.engine.end_after(mark);
                            self.state.ending = true;
                            self.end_deadline = Some(self.end_deadline_from_now());
                            info!("the stream ends once what is buffered has aired");
                        }
                        _ => {
                            self.finish_stream();
                            self.state.ended = true;
                            info!("stream ended by the streamer; nothing was left to air");
                        }
                    }
                }
                self.publish_state();
                let _ = reply.send(());
            }
            Control::SetKeepHistory(keep) => {
                info!(keep, "rolling buffer setting changed");
                self.engine.set_keep_history(keep);
                self.publish_state();
            }
            Control::Resume(reply) => {
                if self.state.ended {
                    // The encoder may have kept sending while ended. None of that
                    // may air: the new broadcast starts from what arrives from now.
                    self.engine.discard();
                    self.clear_end();
                    self.state.ended = false;
                    info!("resuming the broadcast");
                } else if self.state.ending && !self.restart_after_end {
                    // Changed their mind before the end aired: keep broadcasting.
                    self.engine.cancel_end();
                    self.clear_end();
                    info!("the stream keeps going; End stream was cancelled");
                }
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
                // The previous stream was stopped on purpose, so this is a new one.
                let new_stream = self.ingest_ended.is_some() && self.encoder_stopped;
                if result.is_ok() {
                    if new_stream && self.egress_running && !self.state.ended {
                        // The previous stream's end is still airing. Its broadcast
                        // ends once it has, and this stream gets a new one, as when
                        // streaming straight to the destination. Continuing the old
                        // broadcast would leave it without data for as long as the
                        // encoder was stopped, which destinations take badly.
                        if !self.state.ending
                            && !self.restart_after_end
                            && let Some(mark) = self.engine.last_seq()
                        {
                            self.engine.end_after(mark);
                            self.end_deadline = Some(self.end_deadline_from_now());
                        }
                        self.restart_after_end = true;
                        info!(
                            "new encoder stream; it starts a new broadcast once the previous one has aired"
                        );
                    }
                    if let Some(old) = self.publisher.take() {
                        // The encoder reconnected while its old connection still
                        // looked open: close that one and carry on with this one.
                        info!("the encoder reconnected; closing its previous, silent connection");
                        old.close.notify_one();
                        self.engine.ingest_end(now);
                        self.encoder_stopped = false;
                    }
                    self.engine.ingest_start(now);
                    self.ingest_ended = None;
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
                    // Stopping and starting the stream in the encoder is the
                    // natural way to go live again after "end stream". An encoder
                    // reconnecting by itself after its connection dropped (OBS does
                    // this automatically) is not: that stays ended.
                    if self.state.ended {
                        if self.encoder_stopped {
                            self.state.ended = false;
                            info!("new encoder stream; broadcasting again");
                        } else {
                            info!("encoder reconnected; the stream stays ended until resumed");
                        }
                    }
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
            } => {
                if let Some(p) = self.publisher_mut(conn) {
                    p.last_seen = Instant::now();
                    self.engine.ingest(now, kind, ts, payload);
                }
            }
            Event::IngestMetadata { conn, payload } => {
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
                    self.encoder_stopped = unpublished;
                    self.engine.ingest_end(now);
                    self.ingest_ended = Some(Instant::now());
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
                if let Some(e) = error.filter(|_| self.egress_running) {
                    self.state.egress.reconnects += 1;
                    self.state.egress.last_error = Some(clip(e));
                }
                self.publish_state();
            }
            Event::EgressStatus { status, error } => {
                if !self.egress_running && status != EgressStatus::Idle {
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
                "unknown application '{app}' (expected '{want}')"
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

    /// The encoder left more than the grace period ago and did not come back: the
    /// stream is over, apart from airing what is still buffered.
    fn encoder_gone(&self) -> bool {
        self.publisher.is_none()
            && self
                .ingest_ended
                .is_some_and(|t| t.elapsed() >= self.config.encoder_grace)
    }

    /// Like [`Core::finish_stream`], and shows `why` as the destination's last error.
    fn finish_stream_because(&mut self, why: &str) {
        self.state.egress.last_error = Some(why.into());
        self.finish_stream();
        self.publish_state();
    }

    /// Ends the broadcast (if one is running) and forgets what is left of the
    /// stream, so none of it can start a broadcast later.
    fn finish_stream(&mut self) {
        if self.egress_running {
            self.egress_running = false;
            let _ = self.egress_ctl.send(EgressCtl::Stop);
        }
        self.engine.discard();
        self.ingest_ended = None;
        self.clear_end();
    }

    /// Forgets a pending end at the engine's end mark.
    fn clear_end(&mut self) {
        self.state.ending = false;
        self.restart_after_end = false;
        self.end_deadline = None;
    }

    /// When a broadcast told to end at the end mark ends anyway: once all of it
    /// was due to air, plus the slack a slow destination gets.
    fn end_deadline_from_now(&self) -> Instant {
        Instant::now() + Duration::from_micros(self.engine.effective_delay()) + DRAIN_SLACK
    }

    /// Ends the broadcast at the engine's end mark once everything up to it has
    /// been written. Returns true if it did.
    fn end_at_mark(&mut self) -> bool {
        if !self.state.ending && !self.restart_after_end {
            return false;
        }
        let aired = self.engine.end_reached() && self.counters.backlog.load(Ordering::Relaxed) == 0;
        let overdue = self.end_deadline.is_some_and(|d| Instant::now() >= d);
        if !aired && !overdue {
            return false;
        }
        if !aired {
            warn!("the destination stopped taking data; ending the broadcast without the rest");
        }
        if self.restart_after_end {
            info!("the previous stream has aired; ending its broadcast for the new one");
            if self.egress_running {
                self.egress_running = false;
                let _ = self.egress_ctl.send(EgressCtl::Stop);
            }
            // Keeps what the new stream has sent; its broadcast starts when due.
            self.engine.restart_after_end();
            self.clear_end();
        } else {
            info!("everything up to End stream has aired; ending the broadcast");
            self.finish_stream();
            self.state.ended = true;
        }
        self.publish_state();
        true
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
            let _ = self.media_tx.send((self.generation, m));
        }
        // The 250 ms state tick also re-runs this, so connect decisions never stall.
        self.manage_egress(now);
        wake
    }

    fn manage_egress(&mut self, now: u64) {
        let Some(dest) = self.config.destination.clone() else {
            if self.egress_running {
                self.egress_running = false;
                let _ = self.egress_ctl.send(EgressCtl::Stop);
            }
            return;
        };
        if self.end_at_mark() {
            return;
        }
        let gone = self.encoder_gone();
        // OBS stopped the stream on purpose (rather than crashing or losing its
        // connection): there is no reconnect to wait for, so the broadcast ends as
        // soon as the rest of the stream has aired.
        let stopped =
            self.publisher.is_none() && self.ingest_ended.is_some() && self.encoder_stopped;
        if !self.egress_running {
            if gone && (self.state.ended || !self.engine.has_buffered()) {
                // Nothing of this stream is left to air.
                self.finish_stream();
            } else if !self.state.ended && self.engine.output_wanted(now) {
                // A stream that ended before any of it was due (shorter than the
                // delay) still airs, on schedule.
                let Some(target) = self.target(&dest) else {
                    if gone {
                        // Nor may it air later: once the settings are fixed, that
                        // would start a broadcast of a stream that has finished.
                        info!(
                            "encoder gone and the destination is not set up; discarding the stream"
                        );
                        self.finish_stream();
                    }
                    return;
                };
                self.egress_running = true;
                self.egress_started = Instant::now();
                let _ = self.egress_ctl.send(EgressCtl::Start(target));
            }
            return;
        }
        if !gone && !stopped {
            return;
        }
        // End the broadcast once everything buffered has been written to the
        // destination (not just queued for it: the stop would otherwise cut off
        // the last messages).
        let connected = self.engine.output_is_connected();
        if connected && self.engine.drained() && self.counters.backlog.load(Ordering::Relaxed) == 0
        {
            info!("the stream is over and has all aired; ending the broadcast");
            self.finish_stream();
        } else if !connected {
            // Unreachable. While the encoder may come back, and until the
            // destination has had the grace period to answer, a reconnect may
            // still air the rest; after that, it would start a new broadcast of a
            // stream that has finished.
            if gone && self.egress_started.elapsed() >= self.config.encoder_grace {
                info!("encoder gone and the destination is not connected; ending the broadcast");
                self.finish_stream_because(
                    "The stream ended while the destination could not be reached; \
                     what had not aired yet was discarded.",
                );
            }
        } else if self.ingest_ended.is_some_and(|t| {
            let delay = Duration::from_micros(self.engine.effective_delay());
            let due = if stopped {
                delay
            } else {
                self.config.encoder_grace.max(delay)
            };
            t.elapsed() >= due + DRAIN_SLACK
        }) {
            warn!("the destination stopped taking data; ending the broadcast without the rest");
            self.finish_stream_because(
                "The destination stopped taking data, so the broadcast was ended \
                 without the rest of the stream.",
            );
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
