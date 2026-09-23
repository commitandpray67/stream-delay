//! The core task: owns the delay engine and wires ingest, egress and commands together.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use streamdelay_engine::{Engine, EngineError, Kind, OutMsg};
use streamdelay_rtmp::RtmpUrl;
use streamdelay_rtmp::amf0::Amf0Value;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;
use tracing::{info, warn};

use crate::egress::{self, Counters, EgressCtl, Target};
use crate::{
    Control, Destination, DestinationKey, EgressState, EgressStatus, IngestState, RelayConfig,
    RelayState,
};

pub(crate) enum Event {
    IngestPublish {
        conn: u64,
        peer: SocketAddr,
        app: String,
        key: String,
        connect_props: Vec<(String, Amf0Value)>,
        reply: oneshot::Sender<Result<(), String>>,
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
}

struct Core {
    config: RelayConfig,
    engine: Engine,
    clock: Instant,
    publisher: Option<Publisher>,
    /// Key and connect properties of the most recent publisher (for passthrough).
    last_publisher: Option<(String, Vec<(String, Amf0Value)>)>,
    ingest_ended: Option<Instant>,
    egress_ctl: mpsc::UnboundedSender<EgressCtl>,
    media_tx: mpsc::UnboundedSender<(u64, OutMsg)>,
    counters: Arc<Counters>,
    egress_running: bool,
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
        egress_ctl,
        media_tx,
        counters,
        egress_running: false,
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
                    let _ = core.egress_ctl.send(EgressCtl::Stop);
                    // Give the egress a moment to unpublish cleanly.
                    tokio::time::sleep(Duration::from_millis(300)).await;
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
                let _ = reply.send(r);
                self.publish_state();
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
                // Drop the buffer first so nothing buffered can be sent, then close
                // the destination connection, which ends the broadcast.
                self.engine.discard();
                if self.egress_running {
                    self.egress_running = false;
                    let _ = self.egress_ctl.send(EgressCtl::Stop);
                }
                self.state.ended = true;
                info!("stream ended by the streamer; buffered content discarded");
                let _ = reply.send(());
                self.publish_state();
            }
            Control::SetKeepHistory(keep) => {
                info!(keep, "rolling buffer setting changed");
                self.engine.set_keep_history(keep);
                self.publish_state();
            }
            Control::Resume(reply) => {
                if self.state.ended {
                    self.state.ended = false;
                    info!("resuming the broadcast");
                }
                let _ = reply.send(());
                self.publish_state();
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
                reply,
            } => {
                let result = self.accept_publisher(&app, &key);
                if result.is_ok() {
                    self.engine.ingest_start(now);
                    self.ingest_ended = None;
                    self.last_publisher = Some((key, connect_props));
                    self.publisher = Some(Publisher { conn });
                    self.state.ingest.connected = true;
                    self.state.ingest.peer = Some(peer.to_string());
                    self.state.ingest.app = Some(app);
                    self.state.ingest.last_error = None;
                    // Starting a new stream in the encoder is the natural way to
                    // go live again after "end stream".
                    if self.state.ended {
                        self.state.ended = false;
                        info!("new encoder stream; broadcasting again");
                    }
                } else if let Err(e) = &result {
                    self.state.ingest.last_error = Some(e.clone());
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
                if self.is_publisher(conn) {
                    self.engine.ingest(now, kind, ts, payload);
                }
            }
            Event::IngestMetadata { conn, payload } => {
                if self.is_publisher(conn) {
                    self.engine.ingest_metadata(now, payload);
                }
            }
            Event::IngestClosed { conn, error } => {
                if let Some(e) = error {
                    // Shown in the dashboard and dock, so a failing encoder
                    // connection is visible without reading logs.
                    self.state.ingest.last_error = Some(format!("encoder connection failed: {e}"));
                    self.publish_state();
                }
                if self.is_publisher(conn) {
                    self.publisher = None;
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
                if let Some(e) = error {
                    self.state.egress.reconnects += 1;
                    self.state.egress.last_error = Some(e);
                }
                self.publish_state();
            }
            Event::EgressStatus { status, error } => {
                if self.config.destination.is_some() || status != EgressStatus::Idle {
                    self.state.egress.status = status;
                }
                if error.is_some() {
                    self.state.egress.last_error = error;
                }
                self.publish_state();
            }
        }
    }

    fn is_publisher(&self, conn: u64) -> bool {
        self.publisher.as_ref().is_some_and(|p| p.conn == conn)
    }

    fn accept_publisher(&self, app: &str, key: &str) -> Result<(), String> {
        if self.publisher.is_some() {
            return Err("another encoder is already streaming to stream-delay".into());
        }
        if let Some(want) = &self.config.ingest_app
            && want != app
        {
            return Err(format!("unknown application '{app}' (expected '{want}')"));
        }
        if let Some(want) = &self.config.ingest_key
            && want != key
        {
            return Err("wrong stream key for stream-delay".into());
        }
        if matches!(
            self.config.destination,
            Some(Destination {
                key: DestinationKey::Passthrough,
                ..
            })
        ) && key.is_empty()
        {
            return Err("passthrough mode needs the destination stream key in the encoder".into());
        }
        Ok(())
    }

    /// Runs the engine, forwards due messages and manages the egress connection.
    fn pump(&mut self) -> Option<u64> {
        let now = self.now();
        let wake = self.engine.poll(now, &mut self.out);
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
        if !self.egress_running {
            if !self.state.ended && self.engine.output_wanted(now) {
                let Some(target) = self.target(&dest) else {
                    return;
                };
                self.egress_running = true;
                let _ = self.egress_ctl.send(EgressCtl::Start(target));
            }
            return;
        }
        // End the broadcast once the encoder has been gone for the grace period and
        // everything buffered has been sent.
        if self.publisher.is_none()
            && self.engine.drained()
            && self
                .ingest_ended
                .is_some_and(|t| t.elapsed() >= self.config.encoder_grace)
        {
            info!("encoder gone and buffer drained; ending the broadcast");
            self.egress_running = false;
            let _ = self.egress_ctl.send(EgressCtl::Stop);
            // What is left belongs to the finished stream; don't keep it around
            // for a later stream to rewind into.
            self.engine.discard();
            self.ingest_ended = None;
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
