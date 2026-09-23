//! RTMP(S) egress: publishes the delayed stream to the destination and reconnects
//! with backoff when the connection drops.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use streamdelay_engine::{Kind, OutMsg};
use streamdelay_rtmp::RtmpUrl;
use streamdelay_rtmp::amf0::Amf0Value;
use streamdelay_rtmp::session::{ClientConfig, ClientEvent, ClientSession, MediaKind};
use streamdelay_rtmp::url::Scheme;
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::EgressStatus;
use crate::core::Event;
use crate::io::{self, BoxStream};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_BACKOFF: Duration = Duration::from_secs(10);
/// How long a clean unpublish may take before the connection is just dropped.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// Everything needed to connect.
#[derive(Clone, PartialEq)]
pub(crate) struct Target {
    pub url: RtmpUrl,
    pub key: String,
    pub connect_props: Vec<(String, Amf0Value)>,
}

pub(crate) enum EgressCtl {
    Start(Target),
    /// Unpublish cleanly and disconnect.
    Stop,
    /// End the broadcast at once ("end stream"): reset the connection, so what the
    /// OS has not sent yet is discarded rather than delivered.
    Abort,
}

/// A second handle on the destination socket, for [`EgressCtl::Abort`].
struct Aborter(Option<socket2::Socket>);

impl Aborter {
    fn new(tcp: &TcpStream) -> Self {
        Self(socket2::SockRef::from(tcp).try_clone().ok())
    }

    /// Makes closing the connection reset it: the OS then drops data still waiting
    /// to be sent (on a slow upload, seconds of stream) instead of delivering it.
    fn arm(&self) {
        if let Some(s) = &self.0 {
            let _ = s.set_linger(Some(Duration::ZERO));
        }
    }
}

/// Counters shared with the core task.
#[derive(Default)]
pub(crate) struct Counters {
    pub backlog: AtomicU64,
    pub written: AtomicU64,
}

enum RunEnd {
    Stopped,
    Retarget(Target),
    Failed { error: String },
}

pub(crate) async fn run(
    mut ctl: mpsc::UnboundedReceiver<EgressCtl>,
    mut media: mpsc::UnboundedReceiver<(u64, OutMsg)>,
    events: mpsc::UnboundedSender<Event>,
    counters: Arc<Counters>,
) {
    let mut target: Option<Target> = None;
    let mut backoff = Duration::from_secs(1);
    loop {
        let Some(t) = target.clone() else {
            // Idle: wait for a start request, discarding anything stale.
            tokio::select! {
                c = ctl.recv() => match c {
                    Some(EgressCtl::Start(t)) => target = Some(t),
                    Some(EgressCtl::Stop | EgressCtl::Abort) => {}
                    None => return,
                },
                m = media.recv() => {
                    let Some((_, m)) = m else { return };
                    counters.backlog.fetch_sub(m.payload.len() as u64, Ordering::Relaxed);
                }
            }
            continue;
        };

        status(&events, EgressStatus::Connecting, None);
        info!(destination = %t.url.redacted(), "connecting to destination");
        let connected = tokio::time::timeout(CONNECT_TIMEOUT, connect(&t)).await;
        let (stream, session, aborter) = match connected {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                warn!("destination connection failed: {e}");
                target = wait_backoff(
                    &mut ctl,
                    &mut media,
                    &events,
                    &counters,
                    t,
                    &e,
                    &mut backoff,
                )
                .await;
                continue;
            }
            Err(_) => {
                let e = "timed out connecting to the destination".to_string();
                warn!("{e}");
                target = wait_backoff(
                    &mut ctl,
                    &mut media,
                    &events,
                    &counters,
                    t,
                    &e,
                    &mut backoff,
                )
                .await;
                continue;
            }
        };
        info!("publishing to destination");
        let (gen_tx, gen_rx) = tokio::sync::oneshot::channel();
        let _ = events.send(Event::EgressConnected { reply: gen_tx });
        let Ok(generation) = gen_rx.await else { return };
        let started = Instant::now();
        let (end, last_written) = publish(
            stream, session, &aborter, generation, &mut ctl, &mut media, &events, &counters,
        )
        .await;
        // The socket closes once this second handle goes too; don't keep it open
        // through a reconnect backoff.
        drop(aborter);
        let error = match &end {
            RunEnd::Failed { error } => Some(error.clone()),
            _ => None,
        };
        let _ = events.send(Event::EgressDisconnected {
            last_written,
            error: error.clone(),
        });
        match end {
            RunEnd::Stopped => {
                target = None;
                status(&events, EgressStatus::Idle, None);
            }
            RunEnd::Retarget(t) => target = Some(t),
            RunEnd::Failed { error } => {
                warn!("destination connection lost: {error}");
                if started.elapsed() > Duration::from_secs(30) {
                    backoff = Duration::from_secs(1);
                }
                target = wait_backoff(
                    &mut ctl,
                    &mut media,
                    &events,
                    &counters,
                    t,
                    &error,
                    &mut backoff,
                )
                .await;
            }
        }
    }
}

fn status(events: &mpsc::UnboundedSender<Event>, s: EgressStatus, error: Option<String>) {
    let _ = events.send(Event::EgressStatus { status: s, error });
}

/// Sleeps before the next attempt. Returns the target to use next (None if stopped).
async fn wait_backoff(
    ctl: &mut mpsc::UnboundedReceiver<EgressCtl>,
    media: &mut mpsc::UnboundedReceiver<(u64, OutMsg)>,
    events: &mpsc::UnboundedSender<Event>,
    counters: &Counters,
    current: Target,
    error: &str,
    backoff: &mut Duration,
) -> Option<Target> {
    status(events, EgressStatus::Retrying, Some(error.to_string()));
    let sleep = tokio::time::sleep(*backoff);
    *backoff = (*backoff * 2).min(MAX_BACKOFF);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => return Some(current),
            c = ctl.recv() => match c {
                Some(EgressCtl::Start(t)) => return Some(t),
                Some(EgressCtl::Stop | EgressCtl::Abort) | None => {
                    status(events, EgressStatus::Idle, None);
                    return None;
                }
            },
            m = media.recv() => {
                let (_, m) = m?;
                counters.backlog.fetch_sub(m.payload.len() as u64, Ordering::Relaxed);
            }
        }
    }
}

async fn connect(t: &Target) -> Result<(BoxStream, ClientSession, Aborter), String> {
    let tcp = TcpStream::connect((t.url.host.as_str(), t.url.port))
        .await
        .map_err(|e| format!("could not reach {}:{}: {e}", t.url.host, t.url.port))?;
    io::tune(&tcp);
    let aborter = Aborter::new(&tcp);
    let mut stream: BoxStream = match t.url.scheme {
        Scheme::Rtmp => Box::pin(tcp),
        Scheme::Rtmps => {
            let name = rustls::pki_types::ServerName::try_from(t.url.host.clone())
                .map_err(|e| format!("invalid TLS server name: {e}"))?;
            let tls = tokio_rustls::TlsConnector::from(io::tls_config())
                .connect(name, tcp)
                .await
                .map_err(|e| format!("TLS handshake failed: {e}"))?;
            Box::pin(tls)
        }
    };
    let rest = io::client_handshake(&mut stream)
        .await
        .map_err(|e| format!("RTMP handshake failed: {e}"))?;
    let mut cfg = ClientConfig::new(t.url.app.clone(), t.url.tc_url.clone(), t.key.clone());
    cfg.extra_connect_props = t.connect_props.clone();
    let mut session = ClientSession::new(cfg);
    let mut pending = Some(rest);
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let out = session.take_output();
        if !out.is_empty() {
            stream
                .write_all(&out)
                .await
                .map_err(|e| format!("write failed: {e}"))?;
        }
        let data = match pending.take() {
            Some(d) if !d.is_empty() => d.to_vec(),
            _ => {
                let n = stream
                    .read(&mut buf)
                    .await
                    .map_err(|e| format!("read failed: {e}"))?;
                if n == 0 {
                    return Err(
                        "the destination closed the connection (check the stream key)".into(),
                    );
                }
                buf[..n].to_vec()
            }
        };
        // Both possible events end the connect phase, so only the first one matters.
        let events = session.feed(&data).map_err(|e| e.to_string())?;
        if let Some(ev) = events.into_iter().next() {
            match ev {
                ClientEvent::Publishing => {
                    let out = session.take_output();
                    stream
                        .write_all(&out)
                        .await
                        .map_err(|e| format!("write failed: {e}"))?;
                    return Ok((stream, session, aborter));
                }
                ClientEvent::Error { code, description } => {
                    return Err(format!(
                        "destination refused the stream: {code} {description}"
                    ));
                }
            }
        }
    }
}

/// What a control message means for the running connection (`None`: the core
/// has gone, which counts as a stop). Arms `aborter` for an abort.
fn ended_by(c: Option<EgressCtl>, aborter: &Aborter) -> RunEnd {
    match c {
        Some(EgressCtl::Start(t)) => RunEnd::Retarget(t),
        Some(EgressCtl::Abort) => {
            aborter.arm();
            RunEnd::Stopped
        }
        Some(EgressCtl::Stop) | None => RunEnd::Stopped,
    }
}

/// Writes `data` unless a control message arrives first, so stopping never waits
/// on a stalled destination. `Err` carries that message; the write was cut short.
async fn write_or_ctl(
    wr: &mut WriteHalf<BoxStream>,
    data: &[u8],
    ctl: &mut mpsc::UnboundedReceiver<EgressCtl>,
) -> Result<std::io::Result<()>, Option<EgressCtl>> {
    tokio::select! {
        biased;
        c = ctl.recv() => Err(c),
        r = wr.write_all(data) => Ok(r),
    }
}

#[allow(clippy::too_many_arguments)]
async fn publish(
    stream: BoxStream,
    mut session: ClientSession,
    aborter: &Aborter,
    generation: u64,
    ctl: &mut mpsc::UnboundedReceiver<EgressCtl>,
    media: &mut mpsc::UnboundedReceiver<(u64, OutMsg)>,
    events: &mpsc::UnboundedSender<Event>,
    counters: &Counters,
) -> (RunEnd, Option<u64>) {
    status(events, EgressStatus::Live, None);
    let (mut rd, mut wr): (ReadHalf<BoxStream>, WriteHalf<BoxStream>) = tokio::io::split(stream);
    let mut buf = vec![0u8; 16 * 1024];
    let mut last_written: Option<u64> = None;
    let mut batch = Vec::with_capacity(64);
    loop {
        tokio::select! {
            biased;
            c = ctl.recv() => {
                // Unpublish cleanly if the destination still takes data, but don't
                // wait on one that has stalled: dropping the connection also ends
                // the broadcast. An abort skips this, so nothing more is sent.
                if !matches!(c, Some(EgressCtl::Abort)) {
                    session.close();
                    let out = session.take_output();
                    let _ = tokio::time::timeout(CLOSE_TIMEOUT, async {
                        wr.write_all(&out).await?;
                        wr.flush().await?;
                        wr.shutdown().await
                    })
                    .await;
                }
                return (ended_by(c, aborter), last_written);
            }
            n = rd.read(&mut buf) => {
                let n = match n {
                    Ok(0) => return (RunEnd::Failed { error: "the destination closed the connection".into() }, last_written),
                    Ok(n) => n,
                    Err(e) => return (RunEnd::Failed { error: format!("read failed: {e}") }, last_written),
                };
                match session.feed(&buf[..n]) {
                    Ok(evs) => {
                        for ev in evs {
                            if let ClientEvent::Error { code, description } = ev {
                                return (RunEnd::Failed { error: format!("{code} {description}") }, last_written);
                            }
                        }
                    }
                    Err(e) => return (RunEnd::Failed { error: e.to_string() }, last_written),
                }
                let out = session.take_output();
                if !out.is_empty() {
                    match write_or_ctl(&mut wr, &out, ctl).await {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => return (RunEnd::Failed { error: format!("write failed: {e}") }, last_written),
                        Err(c) => return (ended_by(c, aborter), last_written),
                    }
                }
            }
            n = media.recv_many(&mut batch, 64) => {
                if n == 0 {
                    return (RunEnd::Stopped, last_written);
                }
                let mut batch_last = None;
                let mut bytes = 0u64;
                for (g, m) in batch.drain(..) {
                    bytes += m.payload.len() as u64;
                    if g != generation {
                        continue;
                    }
                    match m.kind {
                        Kind::Audio => session.send_media(MediaKind::Audio, m.timestamp, &m.payload),
                        Kind::Video => session.send_media(MediaKind::Video, m.timestamp, &m.payload),
                        Kind::Data => session.send_data(m.timestamp, &m.payload),
                    }
                    if m.seq.is_some() {
                        batch_last = m.seq;
                    }
                }
                let out = session.take_output();
                let result = write_or_ctl(&mut wr, &out, ctl).await;
                counters.backlog.fetch_sub(bytes, Ordering::Relaxed);
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => return (RunEnd::Failed { error: format!("write failed: {e}") }, last_written),
                    // Told to stop mid-write (End stream on a slow upload): the RTMP
                    // stream is cut mid-message, so there is no clean unpublish;
                    // dropping the connection ends the broadcast.
                    Err(c) => return (ended_by(c, aborter), last_written),
                }
                counters.written.fetch_add(out.len() as u64, Ordering::Relaxed);
                if batch_last.is_some() {
                    last_written = batch_last;
                }
            }
        }
    }
}
