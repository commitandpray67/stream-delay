//! RTMP(S) egress: publishes the delayed stream to the destination and reconnects
//! with backoff when the connection drops.

use std::collections::VecDeque;
use std::net::SocketAddr;
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
use crate::sendq::{self, Counted, SendQueue};

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
/// trying again at once would not help and could look like abuse: the waits grow
/// to minutes. A new key or destination is tried at once all the same.
const REFUSED_RETRY_WAITS: [Duration; 4] = [
    Duration::from_secs(10),
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
];
const MAX_REFUSED_RETRY_WAIT: Duration = Duration::from_secs(300);
/// While no address of the destination has answered, the next one is tried after
/// this long (Happy Eyeballs, RFC 8305).
const CONNECT_STAGGER: Duration = Duration::from_millis(250);
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
    /// A dump: media queued before it is dropped unsent (see [`Counters::cut`]),
    /// and the connection is reset if media from before it may still be waiting
    /// in this computer (see `publish`). Answered with [`Event::CutDone`]. The
    /// number of the dump (see [`Counters::cut`]).
    Cut(u64),
}

/// Media for the destination, as the core queues it.
pub(crate) struct Queued {
    /// The connection it was made for (see [`Engine::output_connected`]).
    ///
    /// [`Engine::output_connected`]: streamdelay_engine::Engine::output_connected
    pub generation: u64,
    /// The dumps before it (see [`Counters::cut`]).
    pub cut: u64,
    pub msg: OutMsg,
}

/// A second handle on the destination socket, for [`EgressCtl::Abort`] and to
/// ask the OS what it still holds (see [`sendq`]).
struct Aborter(Option<socket2::Socket>, Arc<AtomicU64>);

impl Aborter {
    /// For `tcp`, whose bytes accepted by the OS `written` counts.
    fn new(tcp: &TcpStream, written: Arc<AtomicU64>) -> Self {
        Self(socket2::SockRef::from(tcp).try_clone().ok(), written)
    }

    /// What the OS holds of what was written; `None` if it cannot tell.
    fn send_queue(&self) -> Option<SendQueue> {
        let sock = self.0.as_ref()?;
        sendq::query(sock, self.1.load(Ordering::Relaxed))
    }

    /// Makes closing the connection reset it: the OS then drops data still waiting
    /// to be sent (on a slow upload, seconds of stream) instead of delivering it.
    fn arm(&self) {
        if let Some(s) = &self.0 {
            let _ = s.set_linger(Some(Duration::ZERO));
        }
    }

    /// The most the OS may hold of what was written, if it cannot say, in bytes.
    fn held_at_most(&self) -> u64 {
        let reported = self
            .0
            .as_ref()
            .and_then(|s| s.send_buffer_size().ok())
            .unwrap_or(0) as u64;
        // What the OS reports is not always all it buffers (Windows sizes its
        // buffer by itself; TLS keeps some too): assume generously more.
        reported.max(OS_BUFFER_AT_LEAST) * 2
    }
}

/// See [`Aborter::held_at_most`].
const OS_BUFFER_AT_LEAST: u64 = 4 * 1024 * 1024;

/// Counters shared with the core task.
#[derive(Default)]
pub(crate) struct Counters {
    /// Media queued for the destination and not written yet, including what is
    /// being written.
    pub backlog: AtomicU64,
    pub written: AtomicU64,
    /// Dumps so far: queued media made before the latest is dropped unsent.
    pub cut: AtomicU64,
}

impl Counters {
    fn drop_queued(&self, q: &Queued) {
        self.backlog
            .fetch_sub(q.msg.payload.len() as u64, Ordering::Relaxed);
    }
}

enum RunEnd {
    Stopped,
    Retarget(Target),
    /// Reset for dump `cut`, which is answered once the connection is closed,
    /// with the last message the destination surely has. Then connect again at
    /// once (see [`EgressCtl::Cut`]).
    CutReset {
        cut: u64,
        delivered: Option<u64>,
    },
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

    /// This failure as it may be logged and shown: see [`scrub`]. Nor does it
    /// quote the values in the URL's query, which may be credentials.
    fn scrubbed(mut self, t: &Target) -> Self {
        self.message = scrub(&self.message, &t.key);
        for secret in t.url.query_secrets() {
            if secret.chars().count() >= 4 {
                self.message = self.message.replace(&secret, "<redacted>");
            }
        }
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

/// Failed attempts in a row: all of them, and refusals among them.
#[derive(Default)]
struct Failures {
    all: usize,
    refused: usize,
}

impl Failures {
    /// How long to wait before the next attempt after `f`. A refusal also says
    /// when that is, since it can be minutes.
    fn wait(&mut self, f: &mut Failure) -> Duration {
        let wait = RETRY_WAITS.get(self.all).copied().unwrap_or(MAX_RETRY_WAIT);
        self.all += 1;
        if !f.refused {
            return wait;
        }
        let wait = REFUSED_RETRY_WAITS
            .get(self.refused)
            .copied()
            .unwrap_or(MAX_REFUSED_RETRY_WAIT);
        self.refused += 1;
        let secs = wait.as_secs();
        let when = if secs < 60 {
            format!("{secs} s")
        } else {
            format!("{} min", secs / 60)
        };
        f.message = format!("{}; trying again in {when}", f.message);
        wait
    }
}

pub(crate) async fn run(
    mut ctl: mpsc::UnboundedReceiver<EgressCtl>,
    mut media: mpsc::UnboundedReceiver<Queued>,
    events: mpsc::UnboundedSender<Event>,
    counters: Arc<Counters>,
    stall_timeout: Duration,
) {
    let mut target: Option<Target> = None;
    let mut failures = Failures::default();
    loop {
        let Some(t) = target.clone() else {
            // Idle: wait for a start request, discarding anything stale.
            tokio::select! {
                c = ctl.recv() => match c {
                    Some(EgressCtl::Start(t)) => {
                        // A new broadcast: earlier failures don't count.
                        failures = Failures::default();
                        target = Some(t);
                    }
                    Some(EgressCtl::Cut(n)) => cut_done(&events, n, false, None),
                    Some(EgressCtl::Stop | EgressCtl::Abort) => {}
                    None => return,
                },
                m = media.recv() => {
                    let Some(q) = m else { return };
                    counters.drop_queued(&q);
                }
            }
            continue;
        };

        status(&events, EgressStatus::Connecting, None);
        info!(destination = %t.url.redacted(), "connecting to destination");
        // Stop and End stream must win over a connection attempt: a publish that
        // completes after them would start a broadcast nobody wants.
        let connected = {
            let connecting = tokio::time::timeout(CONNECT_TIMEOUT, connect(&t, io::tls_config()));
            tokio::pin!(connecting);
            loop {
                tokio::select! {
                    biased;
                    c = ctl.recv() => match c {
                        // Nothing is sent yet, and what is queued was made for
                        // another connection.
                        Some(EgressCtl::Cut(n)) => cut_done(&events, n, false, None),
                        Some(EgressCtl::Start(t)) => break Err(Some(t)),
                        Some(EgressCtl::Stop | EgressCtl::Abort) | None => break Err(None),
                    },
                    r = &mut connecting => break Ok(r),
                }
            }
        };
        let connected = match connected {
            Ok(r) => r,
            Err(next) => {
                if next.is_none() {
                    info!("connection attempt cancelled");
                    stopped(&events, &mut media, &counters);
                }
                target = next;
                continue;
            }
        };
        let (stream, session, aborter) = match connected {
            Ok(Ok(c)) => c,
            Ok(Err(f)) => {
                let mut f = f.scrubbed(&t);
                warn!("destination connection failed: {}", f.message);
                let wait = failures.wait(&mut f);
                target = wait_backoff(
                    &mut ctl, &mut media, &events, &counters, t, &f.message, wait,
                )
                .await;
                continue;
            }
            Err(_) => {
                let mut f = Failure::new("timed out connecting to the destination");
                warn!("{}", f.message);
                let wait = failures.wait(&mut f);
                target = wait_backoff(
                    &mut ctl, &mut media, &events, &counters, t, &f.message, wait,
                )
                .await;
                continue;
            }
        };
        // The destination took the stream: refusals before don't count.
        failures.refused = 0;
        // The destination accepted the stream just as a stop arrived: end it at once.
        let pending = loop {
            match ctl.try_recv() {
                // Nothing sent yet.
                Ok(EgressCtl::Cut(n)) => cut_done(&events, n, false, None),
                Ok(c) => break Some(c),
                Err(_) => break None,
            }
        };
        if let Some(c) = pending {
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
        // A failed connection is reset: what the OS still holds for it is stale,
        // and must not reach the destination later (after a dump, say).
        if matches!(end, RunEnd::Failed(_)) {
            aborter.arm();
        }
        // The socket closes once this second handle goes too; don't keep it open
        // through a reconnect backoff.
        drop(aborter);
        // Reset now: nothing more from before the dump can leave.
        if let RunEnd::CutReset { cut, delivered } = end {
            cut_done(&events, cut, true, delivered);
        }
        let end = match end {
            RunEnd::Failed(f) => RunEnd::Failed(f.scrubbed(&t)),
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
            RunEnd::CutReset { .. } => info!(
                "reset the destination connection for a dump, since it was behind; connecting again"
            ),
            RunEnd::Failed(mut f) => {
                warn!("destination connection lost: {}", f.message);
                if started.elapsed() >= STABLE_CONNECTION {
                    failures.all = 0;
                }
                let wait = failures.wait(&mut f);
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

/// Answers [`EgressCtl::Cut`]: whether the connection was reset, and the last
/// message the destination surely has (see [`Event::CutDone`]).
fn cut_done(events: &mpsc::UnboundedSender<Event>, cut: u64, reset: bool, delivered: Option<u64>) {
    let _ = events.send(Event::CutDone {
        cut,
        reset,
        delivered,
    });
}

/// What one connection wrote: the last message of each batch, with how many
/// bytes had been written once it was. Kept for [`WRITTEN_KEPT`] bytes.
#[derive(Default)]
struct WriteLog {
    total: u64,
    batches: VecDeque<(u64, u64)>,
}

/// How far back [`WriteLog`] reaches, in bytes: past what the OS may hold.
const WRITTEN_KEPT: u64 = 32 * 1024 * 1024;

impl WriteLog {
    fn written(&mut self, bytes: u64, last_seq: Option<u64>) {
        self.total += bytes;
        if let Some(seq) = last_seq {
            self.batches.push_back((seq, self.total));
        }
        while self
            .batches
            .front()
            .is_some_and(|&(_, end)| self.total - end > WRITTEN_KEPT)
        {
            self.batches.pop_front();
        }
    }

    /// The last message surely sent, if at most `unsent` of what was written may
    /// still be waiting in the OS.
    fn surely_sent(&self, unsent: u64) -> Option<u64> {
        let sent = self.total.checked_sub(unsent)?;
        self.batches
            .iter()
            .rev()
            .find(|&&(_, end)| end <= sent)
            .map(|&(seq, _)| seq)
    }
}

/// Reports that the egress has stopped, after dropping the media still queued
/// for the connection that ended, so the backlog it reports is already empty.
fn stopped(
    events: &mpsc::UnboundedSender<Event>,
    media: &mut mpsc::UnboundedReceiver<Queued>,
    counters: &Counters,
) {
    while let Ok(q) = media.try_recv() {
        counters.drop_queued(&q);
    }
    status(events, EgressStatus::Idle, None);
}

/// Sleeps `wait` before the next attempt. Returns the target to use next (None if
/// stopped).
async fn wait_backoff(
    ctl: &mut mpsc::UnboundedReceiver<EgressCtl>,
    media: &mut mpsc::UnboundedReceiver<Queued>,
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
                Some(EgressCtl::Cut(n)) => cut_done(events, n, false, None),
                Some(EgressCtl::Stop | EgressCtl::Abort) | None => {
                    stopped(events, media, counters);
                    return None;
                }
            },
            m = media.recv() => {
                counters.drop_queued(&m?);
            }
        }
    }
}

/// Connects to the first of `addrs` that answers. They are tried alternating
/// between IPv6 and IPv4, the next one after [`CONNECT_STAGGER`] while none has
/// answered, or at once when one fails (Happy Eyeballs, RFC 8305): an address
/// that silently drops the connection (IPv6 without a working route, say) must
/// not hold up the others until the attempt times out.
async fn connect_first(addrs: Vec<SocketAddr>) -> std::io::Result<TcpStream> {
    let first_v6 = addrs.first().is_some_and(SocketAddr::is_ipv6);
    let (mut a, mut b): (VecDeque<_>, VecDeque<_>) =
        addrs.into_iter().partition(|x| x.is_ipv6() == first_v6);
    let mut order = Vec::new();
    while !a.is_empty() || !b.is_empty() {
        order.extend(a.pop_front());
        order.extend(b.pop_front());
    }
    let mut addrs = order.into_iter();
    // Dropping the set cancels the attempts still running.
    let mut attempts = tokio::task::JoinSet::new();
    let mut last_error = None;
    loop {
        // Every round starts the next address: after the stagger, or after one
        // failed.
        if let Some(a) = addrs.next() {
            attempts.spawn(TcpStream::connect(a));
        }
        let more = addrs.len() > 0;
        let Some(done) = (tokio::select! {
            r = attempts.join_next() => r,
            _ = tokio::time::sleep(CONNECT_STAGGER), if more => continue,
        }) else {
            return Err(last_error.unwrap_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "no address found")
            }));
        };
        match done {
            Ok(Ok(tcp)) => return Ok(tcp),
            Ok(Err(e)) => last_error = Some(e),
            Err(e) => last_error = Some(std::io::Error::other(e)),
        }
    }
}

/// Connects and publishes to `t`, over TLS with `tls` for RTMPS.
async fn connect(
    t: &Target,
    tls: Arc<rustls::ClientConfig>,
) -> Result<(BoxStream, ClientSession, Aborter), Failure> {
    let unreachable = |e| {
        Failure::new(format!(
            "could not reach {}:{}: {e}",
            t.url.host, t.url.port
        ))
    };
    let addrs = tokio::net::lookup_host((t.url.host.as_str(), t.url.port))
        .await
        .map_err(unreachable)?;
    let tcp = connect_first(addrs.collect()).await.map_err(unreachable)?;
    io::tune(&tcp);
    let written = Arc::new(AtomicU64::new(0));
    let aborter = Aborter::new(&tcp, written.clone());
    let tcp = Counted::new(tcp, written);
    let mut stream: BoxStream = match t.url.scheme {
        Scheme::Rtmp => Box::pin(tcp),
        Scheme::Rtmps => {
            let name = rustls::pki_types::ServerName::try_from(t.url.host.clone())
                .map_err(|e| Failure::new(format!("invalid TLS server name: {e}")))?;
            let tls = tokio_rustls::TlsConnector::from(tls)
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
        // Answered in `publish`, which resets the connection itself if needed.
        Some(EgressCtl::Cut(cut)) => RunEnd::CutReset {
            cut,
            delivered: None,
        },
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
/// on a stalled destination, and flushes it: once done, all of it is in the OS
/// (TLS keeps none back), which a dump relies on (see [`sendq`]). Fails if the
/// destination takes none of it for
/// `stall_timeout`: a connection that died without the network reporting it
/// (after the computer switched networks, say) otherwise holds the stream until
/// the OS gives up on it, which can take a quarter of an hour.
async fn write_or_ctl(
    wr: &mut WriteHalf<BoxStream>,
    data: &[u8],
    ctl: &mut mpsc::UnboundedReceiver<EgressCtl>,
    stall_timeout: Duration,
) -> Written {
    let stalled = || {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!(
                "the destination took no data for {} s",
                stall_timeout.as_secs()
            ),
        )
    };
    let write = async {
        let mut rest = data;
        while !rest.is_empty() {
            match tokio::time::timeout(stall_timeout, wr.write(rest)).await {
                Ok(Ok(0)) => return Err(std::io::ErrorKind::WriteZero.into()),
                Ok(Ok(n)) => rest = &rest[n..],
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err(stalled()),
            }
        }
        match tokio::time::timeout(stall_timeout, wr.flush()).await {
            Ok(r) => r,
            Err(_) => Err(stalled()),
        }
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
    media: &mut mpsc::UnboundedReceiver<Queued>,
    events: &mpsc::UnboundedSender<Event>,
    counters: &Counters,
) -> (RunEnd, Option<u64>) {
    status(events, EgressStatus::Live, None);
    let (mut rd, mut wr): (ReadHalf<BoxStream>, WriteHalf<BoxStream>) = tokio::io::split(stream);
    let mut buf = vec![0u8; 16 * 1024];
    let mut last_written: Option<u64> = None;
    let mut log = WriteLog::default();
    let mut batch = Vec::with_capacity(64);
    // A dump while media from before it may still be waiting in this computer,
    // in a write under way or unsent in the OS: the connection is reset,
    // dropping it. Of what was written, the destination surely has only what
    // it acknowledged.
    let reset_for_cut = |cut: u64, log: &WriteLog| {
        let held = match aborter.send_queue() {
            Some(q) => q.unsent + q.unacked,
            None => aborter.held_at_most(),
        };
        aborter.arm();
        RunEnd::CutReset {
            cut,
            delivered: log.surely_sent(held),
        }
    };
    loop {
        tokio::select! {
            biased;
            c = ctl.recv() => {
                if let Some(EgressCtl::Cut(cut)) = c {
                    // Some of what was written has not left (or the OS cannot
                    // say).
                    if !matches!(aborter.send_queue(), Some(SendQueue { unsent: 0, .. })) {
                        return (reset_for_cut(cut, &log), last_written);
                    }
                    // Nothing from before the dump is waiting here: what was
                    // written is on its way, and what is queued is dropped when
                    // read.
                    cut_done(events, cut, false, last_written);
                    continue;
                }
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
                        Written::Interrupted(Some(EgressCtl::Cut(cut))) => return (reset_for_cut(cut, &log), last_written),
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
                let cut = counters.cut.load(Ordering::SeqCst);
                for q in batch.drain(..) {
                    bytes += q.msg.payload.len() as u64;
                    // Made for an earlier connection, or taken back by a dump.
                    if q.generation != generation || q.cut != cut {
                        continue;
                    }
                    let m = q.msg;
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
                    // A dump while this media was being written: part of it may
                    // be waiting here yet.
                    Written::Interrupted(Some(EgressCtl::Cut(cut))) => return (reset_for_cut(cut, &log), last_written),
                    // Told to stop mid-write (End stream on a slow upload): the RTMP
                    // stream is cut mid-message, so there is no clean unpublish;
                    // dropping the connection ends the broadcast.
                    Written::Interrupted(c) => return (ended_by(c, aborter), last_written),
                }
                counters.written.fetch_add(out.len() as u64, Ordering::Relaxed);
                log.written(out.len() as u64, batch_last);
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
    use streamdelay_rtmp::session::{ServerConfig, ServerEvent, ServerSession};

    use super::*;

    /// A connection being published on, in a task.
    struct Publishing {
        ctl: mpsc::UnboundedSender<EgressCtl>,
        media: mpsc::UnboundedSender<Queued>,
        events: mpsc::UnboundedReceiver<Event>,
        counters: Arc<Counters>,
        task: tokio::task::JoinHandle<(RunEnd, Option<u64>)>,
    }

    impl Publishing {
        fn start(stream: BoxStream, aborter: Aborter) -> Self {
            Self::start_with(stream, publishing_session(), aborter)
        }

        /// Publishing on `session`, which the destination has accepted.
        fn start_with(stream: BoxStream, session: ClientSession, aborter: Aborter) -> Self {
            let (ctl, mut ctl_rx) = mpsc::unbounded_channel();
            let (media, mut media_rx) = mpsc::unbounded_channel();
            let (events_tx, events) = mpsc::unbounded_channel();
            let counters = Arc::new(Counters::default());
            let c = counters.clone();
            let task = tokio::spawn(async move {
                publish(
                    stream,
                    session,
                    &aborter,
                    1,
                    Duration::from_secs(30),
                    &mut ctl_rx,
                    &mut media_rx,
                    &events_tx,
                    &c,
                )
                .await
            });
            Self {
                ctl,
                media,
                events,
                counters,
                task,
            }
        }

        /// Queues frame `seq` of `size` bytes, made before dump `cut`.
        fn frame(&self, seq: u64, size: usize, cut: u64) {
            self.counters
                .backlog
                .fetch_add(size as u64, Ordering::Relaxed);
            let msg = OutMsg {
                kind: Kind::Video,
                timestamp: seq as u32 * 33,
                payload: Bytes::from(vec![0u8; size]),
                seq: Some(seq),
            };
            self.media
                .send(Queued {
                    generation: 1,
                    cut,
                    msg,
                })
                .unwrap();
        }

        /// What the core does for dump 1.
        fn dump(&self) {
            self.counters.cut.store(1, Ordering::SeqCst);
            self.ctl.send(EgressCtl::Cut(1)).unwrap();
        }

        async fn until_written(&self) {
            while self.counters.backlog.load(Ordering::Relaxed) > 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }

        async fn ended(&mut self) -> RunEnd {
            tokio::time::timeout(Duration::from_secs(5), &mut self.task)
                .await
                .expect("still publishing")
                .unwrap()
                .0
        }
    }

    /// A session the destination has accepted the stream on.
    fn publishing_session() -> ClientSession {
        let mut client = ClientSession::new(ClientConfig::new("app", "rtmp://x/app", "k"));
        let mut server = ServerSession::new(ServerConfig::default());
        for _ in 0..10 {
            let events = server.feed(&client.take_output()).unwrap();
            if events
                .iter()
                .any(|e| matches!(e, ServerEvent::PublishRequest { .. }))
            {
                server.accept_publish();
            }
            client.feed(&server.take_output()).unwrap();
            if client.is_publishing() {
                client.take_output();
                return client;
            }
        }
        panic!("the stream was never accepted");
    }

    /// A connected pair: the relay's end, ready to publish on, and the
    /// destination's.
    async fn connection(
        sndbuf: Option<usize>,
        rcvbuf: Option<usize>,
    ) -> (BoxStream, Aborter, TcpStream) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        if let Some(size) = rcvbuf {
            socket2::SockRef::from(&listener)
                .set_recv_buffer_size(size)
                .unwrap();
        }
        let tcp = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        if let Some(size) = sndbuf {
            socket2::SockRef::from(&tcp)
                .set_send_buffer_size(size)
                .unwrap();
        }
        let (far, _) = listener.accept().await.unwrap();
        let written = Arc::new(AtomicU64::new(0));
        let aborter = Aborter::new(&tcp, written.clone());
        (Box::pin(Counted::new(tcp, written)), aborter, far)
    }

    /// Client settings trusting the test CA, and a server's with its `localhost`
    /// certificate (see `testdata/README.md`).
    fn test_tls() -> (Arc<rustls::ClientConfig>, Arc<rustls::ServerConfig>) {
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
        let read = |name: &str| {
            std::fs::read(format!("{}/testdata/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
        };
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from(read("test-ca.der")))
            .unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let client = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(read("localhost.key.der")));
        let server = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![CertificateDer::from(read("localhost.der"))], key)
            .unwrap();
        (Arc::new(client), Arc::new(server))
    }

    /// The destination's end of an RTMPS connection, with its RTMP session.
    struct Destination {
        tls: tokio_rustls::server::TlsStream<TcpStream>,
        session: ServerSession,
        key: String,
    }

    impl Destination {
        /// Reads until `count` media messages have come; returns their sizes.
        async fn media(&mut self, count: usize) -> Vec<usize> {
            let mut sizes = Vec::new();
            let mut buf = vec![0u8; 64 * 1024];
            while sizes.len() < count {
                let n = tokio::time::timeout(Duration::from_secs(5), self.tls.read(&mut buf))
                    .await
                    .expect("no media came")
                    .unwrap();
                assert!(n > 0, "the relay closed the connection");
                for ev in self.session.feed(&buf[..n]).unwrap() {
                    if let ServerEvent::Media { payload, .. } = ev {
                        sizes.push(payload.len());
                    }
                }
            }
            sizes
        }
    }

    /// An RTMPS destination on `localhost` with the test certificate. It takes one
    /// stream (`None` if the relay gave up on the TLS handshake).
    async fn rtmps_destination(
        tls: Arc<rustls::ServerConfig>,
        rcvbuf: Option<usize>,
    ) -> (Target, tokio::task::JoinHandle<Option<Destination>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        if let Some(size) = rcvbuf {
            socket2::SockRef::from(&listener)
                .set_recv_buffer_size(size)
                .unwrap();
        }
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut tls = tokio_rustls::TlsAcceptor::from(tls)
                .accept(tcp)
                .await
                .ok()?;
            let mut data = io::server_handshake(&mut tls).await.unwrap().to_vec();
            let mut session = ServerSession::new(ServerConfig::default());
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                let mut key = None;
                for ev in session.feed(&data).unwrap() {
                    if let ServerEvent::PublishRequest { stream_key, .. } = ev {
                        session.accept_publish();
                        key = Some(stream_key);
                    }
                }
                tls.write_all(&session.take_output()).await.unwrap();
                tls.flush().await.unwrap();
                if let Some(key) = key {
                    return Some(Destination { tls, session, key });
                }
                let n = tls.read(&mut buf).await.unwrap();
                assert!(n > 0, "the relay closed the connection");
                data = buf[..n].to_vec();
            }
        });
        let target = Target {
            url: RtmpUrl::parse(&format!("rtmps://localhost:{port}/app")).unwrap(),
            key: "k".into(),
            connect_props: Vec::new(),
        };
        (target, task)
    }

    /// Connects to `target` over RTMPS; panics with the reason if it fails.
    async fn connect_rtmps(
        target: &Target,
        tls: Arc<rustls::ClientConfig>,
    ) -> (BoxStream, ClientSession, Aborter) {
        match connect(target, tls).await {
            Ok(c) => c,
            Err(f) => panic!("{}", f.message),
        }
    }

    #[tokio::test]
    async fn rtmps_publishes_to_a_destination_whose_certificate_checks_out() {
        let (client_tls, server_tls) = test_tls();
        let (target, destination) = rtmps_destination(server_tls.clone(), None).await;
        let (stream, session, aborter) = connect_rtmps(&target, client_tls).await;
        let mut far = destination.await.unwrap().unwrap();
        assert_eq!(far.key, "k");
        let mut p = Publishing::start_with(stream, session, aborter);
        p.frame(1, 10_000, 0);
        p.frame(2, 70_000, 0);
        assert_eq!(far.media(2).await, [10_000, 70_000]);
        p.ctl.send(EgressCtl::Stop).unwrap();
        assert!(matches!(p.ended().await, RunEnd::Stopped));
        // Certificates are checked: the built-in roots, which the real services'
        // certificates are checked against, do not include the test CA.
        let (target, destination) = rtmps_destination(server_tls, None).await;
        match connect(&target, io::tls_config()).await {
            Ok(_) => panic!("an untrusted certificate was accepted"),
            Err(f) => assert!(f.message.contains("TLS handshake failed"), "{}", f.message),
        }
        assert!(destination.await.unwrap().is_none());
    }

    /// Twitch's and YouTube's RTMPS ingest, with a key they refuse (nothing is
    /// streamed): if the refusal comes from the RTMP exchange, or the key is
    /// even taken, the TLS connection and the certificate check against the
    /// built-in roots worked on the real servers, which the local tests above
    /// cannot show. Needs the internet: run by the "Real destinations" workflow.
    #[tokio::test]
    #[ignore = "connects to Twitch and YouTube"]
    async fn real_rtmps_destinations_take_the_tls_connection() {
        for url in [
            "rtmps://live.twitch.tv:443/app",
            "rtmps://a.rtmps.youtube.com:443/live2",
        ] {
            let target = Target {
                url: RtmpUrl::parse(url).unwrap(),
                key: "stream-delay-ci-invalid-key".into(),
                connect_props: Vec::new(),
            };
            let result =
                tokio::time::timeout(Duration::from_secs(30), connect(&target, io::tls_config()))
                    .await
                    .unwrap_or_else(|_| panic!("{url}: no answer in 30 s"));
            match result {
                Ok(_) => eprintln!("{url}: TLS and RTMP connect worked; the key was taken"),
                Err(f) => {
                    eprintln!("{url}: {}", f.message);
                    for failed in ["could not reach", "TLS", "RTMP handshake failed"] {
                        assert!(!f.message.contains(failed), "{url}: {}", f.message);
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn a_dump_over_rtmps_resets_a_connection_that_is_behind() {
        let (client_tls, server_tls) = test_tls();
        let (target, destination) = rtmps_destination(server_tls, Some(4 * 1024)).await;
        let (stream, session, aborter) = connect_rtmps(&target, client_tls).await;
        // The destination reads nothing more.
        let _far = destination.await.unwrap().unwrap();
        let mut p = Publishing::start_with(stream, session, aborter);
        for seq in 1..=20 {
            p.frame(seq, 64_000, 0);
            if tokio::time::timeout(Duration::from_secs(1), p.until_written())
                .await
                .is_err()
            {
                // No room even in the send buffer: a blocked write.
                break;
            }
        }
        p.dump();
        let end = p.ended().await;
        assert!(matches!(end, RunEnd::CutReset { cut: 1, .. }), "not reset");
        while let Ok(ev) = p.events.try_recv() {
            assert!(!matches!(ev, Event::CutDone { .. }), "answered too early");
        }
    }

    #[tokio::test]
    async fn a_dump_over_rtmps_keeps_a_connection_that_has_sent_everything() {
        let (client_tls, server_tls) = test_tls();
        let (target, destination) = rtmps_destination(server_tls, None).await;
        let (stream, session, aborter) = connect_rtmps(&target, client_tls).await;
        let mut far = destination.await.unwrap().unwrap();
        let mut p = Publishing::start_with(stream, session, aborter);
        for seq in 1..=3 {
            p.frame(seq, 10_000, 0);
        }
        assert_eq!(far.media(3).await.len(), 3);
        // Keep reading, as a destination does.
        tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            while let Ok(1..) = far.tls.read(&mut buf).await {}
        });
        p.dump();
        let answer = loop {
            match tokio::time::timeout(Duration::from_secs(5), p.events.recv())
                .await
                .unwrap()
                .unwrap()
            {
                Event::CutDone {
                    cut,
                    reset,
                    delivered,
                } => break (cut, reset, delivered),
                _ => continue,
            }
        };
        assert_eq!(answer, (1, false, Some(3)));
        p.ctl.send(EgressCtl::Stop).unwrap();
        assert!(matches!(p.ended().await, RunEnd::Stopped));
    }

    #[tokio::test]
    async fn an_address_that_never_answers_does_not_hold_up_the_next() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let working = listener.local_addr().unwrap();
        // Not routed anywhere: a connection attempt waits until it times out, as
        // with IPv6 on a network where it does not work.
        let silent: SocketAddr = "10.255.255.1:9".parse().unwrap();
        let started = Instant::now();
        let tcp =
            tokio::time::timeout(Duration::from_secs(3), connect_first(vec![silent, working]))
                .await
                .expect("still waiting on the first address")
                .unwrap();
        assert_eq!(tcp.peer_addr().unwrap(), working);
        assert!(started.elapsed() < Duration::from_secs(1));
        // One that fails is followed at once, and all failing is an error.
        let refused = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        let tcp = connect_first(vec![refused, working]).await.unwrap();
        assert_eq!(tcp.peer_addr().unwrap(), working);
        assert!(connect_first(vec![refused]).await.is_err());
        assert!(connect_first(Vec::new()).await.is_err());
    }

    #[test]
    fn refusals_wait_longer_each_time() {
        let mut failures = Failures::default();
        let mut waits = Vec::new();
        for _ in 0..6 {
            let mut f = Failure {
                message: "destination refused the stream".into(),
                refused: true,
            };
            waits.push(failures.wait(&mut f).as_secs());
            if waits.len() == 4 {
                assert!(
                    f.message.ends_with("; trying again in 2 min"),
                    "{}",
                    f.message
                );
            }
        }
        assert_eq!(waits, [10, 30, 60, 120, 300, 300]);
        // Other failures keep the quick schedule, and say nothing more.
        let mut failures = Failures::default();
        let mut f = Failure::new("could not reach x");
        assert_eq!(failures.wait(&mut f), Duration::ZERO);
        assert_eq!(f.message, "could not reach x");
    }

    #[tokio::test]
    async fn a_dump_resets_the_connection_during_a_blocked_write() {
        // A destination that takes a few bytes and then nothing: the write of
        // one small frame blocks.
        let (stream, far) = tokio::io::duplex(1024);
        let mut p = Publishing::start(Box::pin(stream), Aborter(None, Arc::default()));
        p.frame(1, 4_000, 0);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(p.counters.backlog.load(Ordering::Relaxed) > 0);
        p.dump();
        let end = p.ended().await;
        // Answered once the connection is closed, by the caller.
        assert!(
            matches!(
                end,
                RunEnd::CutReset {
                    cut: 1,
                    delivered: None
                }
            ),
            "not reset"
        );
        drop(far);
    }

    #[tokio::test]
    async fn a_dump_resets_the_connection_when_the_os_has_not_sent_everything() {
        // A destination that reads nothing, and a send buffer large enough
        // that writes complete while it holds what the destination has no room
        // for (how much that is depends on the OS).
        let (stream, aborter, far) = connection(Some(4 * 1024 * 1024), Some(4 * 1024)).await;
        let mut p = Publishing::start(stream, aborter);
        let mut at_far = vec![0u8; 16 * 1024 * 1024];
        for seq in 1.. {
            assert!(seq <= 200, "the destination took everything");
            p.frame(seq, 64_000, 0);
            if tokio::time::timeout(Duration::from_secs(1), p.until_written())
                .await
                .is_err()
            {
                // No room even in the send buffer: a blocked write.
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            let received = far.peek(&mut at_far).await.unwrap() as u64;
            if received < p.counters.written.load(Ordering::Relaxed) {
                break;
            }
        }
        p.dump();
        // Delivered: none of it for sure, since the OS holds some.
        let end = p.ended().await;
        assert!(matches!(end, RunEnd::CutReset { cut: 1, .. }), "not reset");
        while let Ok(ev) = p.events.try_recv() {
            assert!(!matches!(ev, Event::CutDone { .. }), "answered too early");
        }
    }

    #[tokio::test]
    async fn a_dump_keeps_a_connection_that_has_sent_everything() {
        let (stream, aborter, mut far) = connection(None, None).await;
        let received = Arc::new(AtomicU64::new(0));
        let r = received.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            while let Ok(n @ 1..) = far.read(&mut buf).await {
                r.fetch_add(n as u64, Ordering::SeqCst);
            }
        });
        let mut p = Publishing::start(stream, aborter);
        for seq in 1..=3 {
            p.frame(seq, 10_000, 0);
        }
        p.until_written().await;
        while received.load(Ordering::SeqCst) < p.counters.written.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        p.dump();
        let answer = loop {
            match tokio::time::timeout(Duration::from_secs(5), p.events.recv())
                .await
                .unwrap()
                .unwrap()
            {
                Event::CutDone {
                    cut,
                    reset,
                    delivered,
                } => break (cut, reset, delivered),
                _ => continue,
            }
        };
        assert_eq!(answer, (1, false, Some(3)));
        // Queued before the dump but read after it: dropped unsent.
        let written = p.counters.written.load(Ordering::Relaxed);
        p.frame(4, 10_000, 0);
        p.until_written().await;
        assert_eq!(p.counters.written.load(Ordering::Relaxed), written);
        // After it: sent.
        p.frame(5, 10_000, 1);
        p.until_written().await;
        assert!(p.counters.written.load(Ordering::Relaxed) > written);
        p.ctl.send(EgressCtl::Stop).unwrap();
        assert!(matches!(p.ended().await, RunEnd::Stopped));
    }

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
            media_tx
                .send(Queued {
                    generation: 0,
                    cut: 0,
                    msg: m,
                })
                .unwrap();
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
