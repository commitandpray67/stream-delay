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

/// Waits before reconnecting after a failed or dropped connection. Every second
/// spent reconnecting adds a second of delay (nothing is skipped), so the first
/// attempts come quickly.
const RETRY_WAITS: [Duration; 5] = [
    Duration::ZERO,
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];
const MAX_RETRY_WAIT: Duration = Duration::from_secs(5);
/// After the destination refused the stream (a wrong stream key, for example),
/// trying again at once would not help and could look like abuse.
const REFUSED_RETRY_WAIT: Duration = Duration::from_secs(10);
/// A connection that stayed up this long was working: when it drops, the next
/// attempt comes at once again.
const STABLE_CONNECTION: Duration = Duration::from_secs(10);
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
    Failed(Failure),
}

/// Why a connection failed or ended.
struct Failure {
    message: String,
    /// The destination refused the stream, rather than being unreachable.
    refused: bool,
}

impl Failure {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            refused: false,
        }
    }

    /// This failure as it may be logged and shown: see [`scrub`].
    fn scrubbed(mut self, key: &str) -> Self {
        self.message = scrub(&self.message, key);
        self
    }
}

/// `text` (which may quote what the destination replied) without the stream key
/// the connection used, which a destination may repeat when refusing it, and
/// without control characters. A key shorter than 4 characters is replaced only
/// where it stands on its own, so it does not mangle the words around it.
pub(crate) fn scrub(text: &str, key: &str) -> String {
    let mut out: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if key.is_empty() {
        return out;
    }
    if key.chars().count() < 4 {
        return replace_word(&out, key, "<stream key>");
    }
    // Percent-encoded, with upper- and lower-case hex digits.
    let encoded = |lower: bool| -> String {
        key.bytes()
            .map(|b| match b {
                b if b.is_ascii_alphanumeric() || b"-._~".contains(&b) => (b as char).to_string(),
                b if lower => format!("%{b:02x}"),
                b => format!("%{b:02X}"),
            })
            .collect()
    };
    for form in [key.to_string(), encoded(false), encoded(true)] {
        out = out.replace(&form, "<stream key>");
    }
    out
}

/// Replaces the occurrences of `word` in `text` that no letter or digit touches.
fn replace_word(text: &str, word: &str, with: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(word) {
        // What precedes it: in the rest of the text, or already written out.
        let before = rest[..i].chars().last().or_else(|| out.chars().last());
        let after = rest[i + word.len()..].chars().next();
        let alone =
            !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric);
        out.push_str(&rest[..i]);
        out.push_str(if alone { with } else { word });
        rest = &rest[i + word.len()..];
    }
    out.push_str(rest);
    out
}

/// How long to wait before the next attempt, counting `failures` in a row.
fn retry_wait(failures: &mut usize, refused: bool) -> Duration {
    let wait = RETRY_WAITS
        .get(*failures)
        .copied()
        .unwrap_or(MAX_RETRY_WAIT);
    *failures += 1;
    if refused {
        wait.max(REFUSED_RETRY_WAIT)
    } else {
        wait
    }
}

pub(crate) async fn run(
    mut ctl: mpsc::UnboundedReceiver<EgressCtl>,
    mut media: mpsc::UnboundedReceiver<(u64, OutMsg)>,
    events: mpsc::UnboundedSender<Event>,
    counters: Arc<Counters>,
    stall_timeout: Duration,
) {
    let mut target: Option<Target> = None;
    // Failed attempts in a row.
    let mut failures = 0usize;
    loop {
        let Some(t) = target.clone() else {
            // Idle: wait for a start request, discarding anything stale.
            tokio::select! {
                c = ctl.recv() => match c {
                    Some(EgressCtl::Start(t)) => {
                        // A new broadcast: earlier failures don't count.
                        failures = 0;
                        target = Some(t);
                    }
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
        // Stop and End stream must win over a connection attempt: a publish that
        // completes after them would start a broadcast nobody wants.
        let connected = tokio::select! {
            biased;
            c = ctl.recv() => {
                target = match c {
                    Some(EgressCtl::Start(t)) => Some(t),
                    Some(EgressCtl::Stop | EgressCtl::Abort) | None => {
                        info!("connection attempt cancelled");
                        stopped(&events, &mut media, &counters);
                        None
                    }
                };
                continue;
            }
            r = tokio::time::timeout(CONNECT_TIMEOUT, connect(&t)) => r,
        };
        let (stream, session, aborter) = match connected {
            Ok(Ok(c)) => c,
            Ok(Err(f)) => {
                let f = f.scrubbed(&t.key);
                warn!("destination connection failed: {}", f.message);
                let wait = retry_wait(&mut failures, f.refused);
                target = wait_backoff(
                    &mut ctl, &mut media, &events, &counters, t, &f.message, wait,
                )
                .await;
                continue;
            }
            Err(_) => {
                let e = "timed out connecting to the destination";
                warn!("{e}");
                let wait = retry_wait(&mut failures, false);
                target = wait_backoff(&mut ctl, &mut media, &events, &counters, t, e, wait).await;
                continue;
            }
        };
        // The destination accepted the stream just as a stop arrived: end it at once.
        if let Ok(c) = ctl.try_recv() {
            let end = ended_by(Some(c), &aborter);
            drop(stream);
            drop(aborter);
            target = match end {
                RunEnd::Retarget(t) => Some(t),
                _ => {
                    stopped(&events, &mut media, &counters);
                    None
                }
            };
            continue;
        }
        info!("publishing to destination");
        let (gen_tx, gen_rx) = tokio::sync::oneshot::channel();
        let _ = events.send(Event::EgressConnected { reply: gen_tx });
        let Ok(generation) = gen_rx.await else { return };
        let started = Instant::now();
        let (end, last_written) = publish(
            stream,
            session,
            &aborter,
            generation,
            stall_timeout,
            &mut ctl,
            &mut media,
            &events,
            &counters,
        )
        .await;
        // The socket closes once this second handle goes too; don't keep it open
        // through a reconnect backoff.
        drop(aborter);
        let end = match end {
            RunEnd::Failed(f) => RunEnd::Failed(f.scrubbed(&t.key)),
            other => other,
        };
        let error = match &end {
            RunEnd::Failed(f) => Some(f.message.clone()),
            _ => None,
        };
        let _ = events.send(Event::EgressDisconnected {
            last_written,
            error: error.clone(),
        });
        match end {
            RunEnd::Stopped => {
                target = None;
                stopped(&events, &mut media, &counters);
            }
            RunEnd::Retarget(t) => target = Some(t),
            RunEnd::Failed(f) => {
                warn!("destination connection lost: {}", f.message);
                if started.elapsed() >= STABLE_CONNECTION {
                    failures = 0;
                }
                let wait = retry_wait(&mut failures, f.refused);
                target = wait_backoff(
                    &mut ctl, &mut media, &events, &counters, t, &f.message, wait,
                )
                .await;
            }
        }
    }
}

fn status(events: &mpsc::UnboundedSender<Event>, s: EgressStatus, error: Option<String>) {
    let _ = events.send(Event::EgressStatus { status: s, error });
}

/// Reports that the egress has stopped, after dropping the media still queued
/// for the connection that ended, so the backlog it reports is already empty.
fn stopped(
    events: &mpsc::UnboundedSender<Event>,
    media: &mut mpsc::UnboundedReceiver<(u64, OutMsg)>,
    counters: &Counters,
) {
    while let Ok((_, m)) = media.try_recv() {
        counters
            .backlog
            .fetch_sub(m.payload.len() as u64, Ordering::Relaxed);
    }
    status(events, EgressStatus::Idle, None);
}

/// Sleeps `wait` before the next attempt. Returns the target to use next (None if
/// stopped).
async fn wait_backoff(
    ctl: &mut mpsc::UnboundedReceiver<EgressCtl>,
    media: &mut mpsc::UnboundedReceiver<(u64, OutMsg)>,
    events: &mpsc::UnboundedSender<Event>,
    counters: &Counters,
    current: Target,
    error: &str,
    wait: Duration,
) -> Option<Target> {
    status(events, EgressStatus::Retrying, Some(error.to_string()));
    let sleep = tokio::time::sleep(wait);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            _ = &mut sleep => return Some(current),
            c = ctl.recv() => match c {
                Some(EgressCtl::Start(t)) => return Some(t),
                Some(EgressCtl::Stop | EgressCtl::Abort) | None => {
                    stopped(events, media, counters);
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

async fn connect(t: &Target) -> Result<(BoxStream, ClientSession, Aborter), Failure> {
    let tcp = TcpStream::connect((t.url.host.as_str(), t.url.port))
        .await
        .map_err(|e| {
            Failure::new(format!(
                "could not reach {}:{}: {e}",
                t.url.host, t.url.port
            ))
        })?;
    io::tune(&tcp);
    let aborter = Aborter::new(&tcp);
    let mut stream: BoxStream = match t.url.scheme {
        Scheme::Rtmp => Box::pin(tcp),
        Scheme::Rtmps => {
            let name = rustls::pki_types::ServerName::try_from(t.url.host.clone())
                .map_err(|e| Failure::new(format!("invalid TLS server name: {e}")))?;
            let tls = tokio_rustls::TlsConnector::from(io::tls_config())
                .connect(name, tcp)
                .await
                .map_err(|e| Failure::new(format!("TLS handshake failed: {e}")))?;
            Box::pin(tls)
        }
    };
    let rest = io::client_handshake(&mut stream)
        .await
        .map_err(|e| Failure::new(format!("RTMP handshake failed: {e}")))?;
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
                .map_err(|e| Failure::new(format!("write failed: {e}")))?;
        }
        let data = match pending.take() {
            Some(d) if !d.is_empty() => d.to_vec(),
            _ => {
                let n = stream
                    .read(&mut buf)
                    .await
                    .map_err(|e| Failure::new(format!("read failed: {e}")))?;
                if n == 0 {
                    // What servers do with a wrong stream key, too.
                    return Err(Failure {
                        message: "the destination closed the connection (check the stream key)"
                            .into(),
                        refused: true,
                    });
                }
                buf[..n].to_vec()
            }
        };
        // Both possible events end the connect phase, so only the first one matters.
        let events = session
            .feed(&data)
            .map_err(|e| Failure::new(e.to_string()))?;
        if let Some(ev) = events.into_iter().next() {
            match ev {
                ClientEvent::Publishing => {
                    let out = session.take_output();
                    stream
                        .write_all(&out)
                        .await
                        .map_err(|e| Failure::new(format!("write failed: {e}")))?;
                    return Ok((stream, session, aborter));
                }
                ClientEvent::Error { code, description } => {
                    return Err(Failure {
                        message: format!("destination refused the stream: {code} {description}"),
                        refused: true,
                    });
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

/// Outcome of [`write_or_ctl`].
enum Written {
    Done,
    Failed(std::io::Error),
    /// A control message arrived first and the write was cut short.
    Interrupted(Option<EgressCtl>),
}

/// Writes `data` unless a control message arrives first, so stopping never waits
/// on a stalled destination. Fails if the destination takes none of it for
/// `stall_timeout`: a connection that died without the network reporting it
/// (after the computer switched networks, say) otherwise holds the stream until
/// the OS gives up on it, which can take a quarter of an hour.
async fn write_or_ctl(
    wr: &mut WriteHalf<BoxStream>,
    data: &[u8],
    ctl: &mut mpsc::UnboundedReceiver<EgressCtl>,
    stall_timeout: Duration,
) -> Written {
    let write = async {
        let mut rest = data;
        while !rest.is_empty() {
            match tokio::time::timeout(stall_timeout, wr.write(rest)).await {
                Ok(Ok(0)) => return Err(std::io::ErrorKind::WriteZero.into()),
                Ok(Ok(n)) => rest = &rest[n..],
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!(
                            "the destination took no data for {} s",
                            stall_timeout.as_secs()
                        ),
                    ));
                }
            }
        }
        Ok(())
    };
    tokio::select! {
        biased;
        c = ctl.recv() => Written::Interrupted(c),
        r = write => match r {
            Ok(()) => Written::Done,
            Err(e) => Written::Failed(e),
        },
    }
}

#[allow(clippy::too_many_arguments)]
async fn publish(
    stream: BoxStream,
    mut session: ClientSession,
    aborter: &Aborter,
    generation: u64,
    stall_timeout: Duration,
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
                    Ok(0) => return (RunEnd::Failed(Failure::new("the destination closed the connection")), last_written),
                    Ok(n) => n,
                    Err(e) => return (RunEnd::Failed(Failure::new(format!("read failed: {e}"))), last_written),
                };
                match session.feed(&buf[..n]) {
                    Ok(evs) => {
                        for ev in evs {
                            if let ClientEvent::Error { code, description } = ev {
                                return (RunEnd::Failed(Failure { message: format!("{code} {description}"), refused: true }), last_written);
                            }
                        }
                    }
                    Err(e) => return (RunEnd::Failed(Failure::new(e.to_string())), last_written),
                }
                let out = session.take_output();
                if !out.is_empty() {
                    match write_or_ctl(&mut wr, &out, ctl, stall_timeout).await {
                        Written::Done => {}
                        Written::Failed(e) => return (RunEnd::Failed(Failure::new(format!("write failed: {e}"))), last_written),
                        Written::Interrupted(c) => return (ended_by(c, aborter), last_written),
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
                let result = write_or_ctl(&mut wr, &out, ctl, stall_timeout).await;
                counters.backlog.fetch_sub(bytes, Ordering::Relaxed);
                match result {
                    Written::Done => {}
                    Written::Failed(e) => return (RunEnd::Failed(Failure::new(format!("write failed: {e}"))), last_written),
                    // Told to stop mid-write (End stream on a slow upload): the RTMP
                    // stream is cut mid-message, so there is no clean unpublish;
                    // dropping the connection ends the broadcast.
                    Written::Interrupted(c) => return (ended_by(c, aborter), last_written),
                }
                counters.written.fetch_add(out.len() as u64, Ordering::Relaxed);
                if batch_last.is_some() {
                    last_written = batch_last;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    #[test]
    fn errors_never_carry_the_stream_key() {
        let key = "live_123_Ab+c/d";
        let r = scrub(
            "NetStream.Publish.BadName bad key live_123_Ab+c/d\n\x1b[31m",
            key,
        );
        assert_eq!(r, "NetStream.Publish.BadName bad key <stream key>  [31m");
        assert_eq!(
            scrub("stream live_123_Ab%2Bc%2Fd refused", key),
            "stream <stream key> refused"
        );
        assert_eq!(
            scrub("stream live_123_Ab%2bc%2fd refused", key),
            "stream <stream key> refused"
        );
        // Short keys are replaced where they stand alone, not inside words.
        assert_eq!(
            scrub("bad key k (stalled)", "k"),
            "bad key <stream key> (stalled)"
        );
        assert_eq!(scrub("k", "k"), "<stream key>");
        assert_eq!(
            scrub("abc,abcd abc", "abc"),
            "<stream key>,abcd <stream key>"
        );
    }

    #[tokio::test]
    async fn stopping_leaves_no_backlog_behind() {
        let (ctl_tx, mut ctl) = mpsc::unbounded_channel();
        let (media_tx, mut media) = mpsc::unbounded_channel();
        let (events, mut events_rx) = mpsc::unbounded_channel();
        let counters = Counters::default();
        // Media queued for a connection that has gone, and then a stop.
        for i in 0..100u32 {
            counters.backlog.fetch_add(1000, Ordering::Relaxed);
            let m = OutMsg {
                kind: Kind::Video,
                timestamp: i,
                payload: Bytes::from(vec![0u8; 1000]),
                seq: Some(u64::from(i)),
            };
            media_tx.send((0, m)).unwrap();
        }
        ctl_tx.send(EgressCtl::Stop).unwrap();
        let target = Target {
            url: RtmpUrl::parse("rtmp://127.0.0.1/app").unwrap(),
            key: "k".into(),
            connect_props: Vec::new(),
        };
        let next = wait_backoff(
            &mut ctl,
            &mut media,
            &events,
            &counters,
            target,
            "unreachable",
            Duration::from_secs(60),
        )
        .await;
        assert!(next.is_none());
        // Idle is reported with the queue already emptied, so the state published
        // for it shows no backlog.
        assert_eq!(counters.backlog.load(Ordering::Relaxed), 0);
        let mut last = None;
        while let Ok(ev) = events_rx.try_recv() {
            if let Event::EgressStatus { status, .. } = ev {
                last = Some(status);
            }
        }
        assert_eq!(last, Some(EgressStatus::Idle));
    }
}
