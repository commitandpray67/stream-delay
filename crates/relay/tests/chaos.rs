//! Chaos tests: network faults between the relay and the destination, and encoder
//! crashes. A fault-injecting TCP proxy sits between the relay and the sink.

mod common;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use streamdelay_relay::{DelayMode, DestinationKey, EgressStatus, Phase};
use streamdelay_rtmp::session::MediaKind;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A TCP proxy that can reset every connection or stop forwarding on demand.
struct FaultProxy {
    addr: SocketAddr,
    /// Bumped to abort all current connections with a TCP reset.
    generation: Arc<AtomicU64>,
    /// While set, no bytes are forwarded in either direction.
    stalled: Arc<AtomicBool>,
    /// For each connection, in order (as the sink numbers them), what came from
    /// the relay.
    conns: Arc<Mutex<Vec<Arc<Upstream>>>>,
}

/// What one proxy connection received from the relay, in bytes.
#[derive(Default)]
struct Upstream {
    forwarded: AtomicU64,
    /// Waiting in the proxy's receive buffer, as last seen while stalled.
    waiting: AtomicU64,
    /// Once this much is forwarded, forwarding pauses for a second (see
    /// [`FaultProxy::pause_at`]).
    pause_at: AtomicU64,
}

impl Upstream {
    /// Everything that has left the relay's computer on this connection.
    fn arrived(&self) -> u64 {
        self.forwarded.load(Ordering::SeqCst) + self.waiting.load(Ordering::SeqCst)
    }
}

impl FaultProxy {
    async fn start(target: SocketAddr) -> Self {
        Self::start_with(target, None).await
    }

    /// With a receive buffer of `rcvbuf` bytes towards the relay: what the relay
    /// has written but the stalled proxy not yet read, like a destination's.
    async fn start_with(target: SocketAddr, rcvbuf: Option<usize>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        if let Some(size) = rcvbuf {
            // Connections accepted later take it over.
            socket2::SockRef::from(&listener)
                .set_recv_buffer_size(size)
                .unwrap();
        }
        let addr = listener.local_addr().unwrap();
        let generation = Arc::new(AtomicU64::new(0));
        let stalled = Arc::new(AtomicBool::new(false));
        let conns = Arc::new(Mutex::new(Vec::new()));
        let (g, st, cs) = (generation.clone(), stalled.clone(), conns.clone());
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let Ok(server) = TcpStream::connect(target).await else {
                    continue;
                };
                let upstream = Arc::new(Upstream {
                    pause_at: AtomicU64::new(u64::MAX),
                    ..Upstream::default()
                });
                cs.lock().unwrap().push(upstream.clone());
                let my_gen = g.load(Ordering::SeqCst);
                let (cr, cw) = client.into_split();
                let (sr, sw) = server.into_split();
                let g1 = g.clone();
                let g2 = g.clone();
                let (s1, s2) = (st.clone(), st.clone());
                tokio::spawn(pump(cr, sw, g1, my_gen, s1, Some(upstream)));
                tokio::spawn(pump(sr, cw, g2, my_gen, s2, None));
            }
        });
        Self {
            addr,
            generation,
            stalled,
            conns,
        }
    }

    /// The current connection: its number and what it received.
    fn current(&self) -> (usize, Arc<Upstream>) {
        let conns = self.conns.lock().unwrap();
        (conns.len() - 1, conns.last().unwrap().clone())
    }

    /// While stalled: what has left the relay's computer on the current
    /// connection, once that stops growing (the proxy's receive buffer is full,
    /// or the relay has no more to send).
    async fn settled(&self) -> (usize, u64) {
        let (conn, up) = self.current();
        let mut last = up.arrived();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let now = up.arrived();
            if now == last {
                return (conn, now);
            }
            assert!(Instant::now() < deadline, "the connection never settled");
            last = now;
        }
    }

    fn reset_all(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    fn stall(&self, on: bool) {
        self.stalled.store(on, Ordering::SeqCst);
    }
}

async fn pump(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    generation: Arc<AtomicU64>,
    my_gen: u64,
    stalled: Arc<AtomicBool>,
    upstream: Option<Arc<Upstream>>,
) {
    let mut buf = vec![0u8; 16 * 1024];
    let mut peek = Vec::new();
    loop {
        if generation.load(Ordering::SeqCst) != my_gen {
            break;
        }
        if stalled.load(Ordering::SeqCst) {
            if let Some(up) = &upstream {
                // Larger than any receive buffer: all that is waiting.
                peek.resize(16 * 1024 * 1024, 0);
                if let Ok(Ok(n)) =
                    tokio::time::timeout(Duration::from_millis(5), from.peek(&mut peek)).await
                {
                    up.waiting.store(n as u64, Ordering::SeqCst);
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            continue;
        }
        let n = tokio::select! {
            r = from.read(&mut buf) => match r {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            },
            _ = tokio::time::sleep(Duration::from_millis(50)) => continue,
        };
        let Some(up) = &upstream else {
            if to.write_all(&buf[..n]).await.is_err() {
                break;
            }
            continue;
        };
        up.waiting.store(0, Ordering::SeqCst);
        let done = up.forwarded.load(Ordering::SeqCst);
        let pause_at = up.pause_at.load(Ordering::SeqCst);
        // Split at the pause, so the sink reads what came before it on its own.
        let split = if (done..done + n as u64).contains(&pause_at) {
            (pause_at - done) as usize
        } else {
            n
        };
        if to.write_all(&buf[..split]).await.is_err() {
            break;
        }
        up.forwarded.fetch_add(split as u64, Ordering::SeqCst);
        if split < n {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if to.write_all(&buf[split..n]).await.is_err() {
                break;
            }
            up.forwarded.fetch_add((n - split) as u64, Ordering::SeqCst);
        }
    }
    // Abortive close so the relay sees a reset rather than a clean shutdown.
    if let Ok(stream) = from.reunite(to) {
        let _ = socket2::SockRef::from(&stream).set_linger(Some(Duration::ZERO));
    }
}

/// Checks per sink connection: output starts on a keyframe and timestamps never go
/// backwards. Returns the frame ids received.
fn check_connections(log: &SinkLog) -> Vec<u32> {
    let mut frames = Vec::new();
    for conn in 0..log.connections {
        let media: Vec<_> = log.media.iter().filter(|m| m.conn == conn).collect();
        let video: Vec<_> = media
            .iter()
            .filter(|m| m.kind == MediaKind::Video)
            .collect();
        let Some(first) = video.iter().find_map(|m| frame_of(&m.payload)) else {
            continue;
        };
        assert_eq!(
            first % 30,
            0,
            "connection {conn} did not start on a keyframe"
        );
        for kind in [MediaKind::Audio, MediaKind::Video] {
            let ts: Vec<u32> = media
                .iter()
                .filter(|m| m.kind == kind)
                .map(|m| m.ts)
                .collect();
            assert!(
                ts.windows(2).all(|w| w[0] <= w[1]),
                "connection {conn}: {kind:?} ts went back"
            );
        }
        frames.extend(video.iter().filter_map(|m| frame_of(&m.payload)));
    }
    frames
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn destination_resets_and_stalls_never_leak_or_corrupt() {
    let (sink, log, _kill) = start_sink().await;
    let proxy = FaultProxy::start(sink).await;
    let relay = start_relay(
        proxy.addr,
        DestinationKey::Fixed("k".into()),
        Duration::from_secs(5),
    )
    .await;
    relay.set_delay(3_000, DelayMode::Rewind).await.unwrap();
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;

    p.stream_for(Duration::from_secs(5)).await;
    proxy.reset_all();
    p.stream_for(Duration::from_secs(4)).await;
    proxy.stall(true);
    p.stream_for(Duration::from_secs(3)).await;
    proxy.stall(false);
    p.stream_for(Duration::from_secs(3)).await;
    proxy.reset_all();
    p.stream_for(Duration::from_secs(6)).await;

    {
        let l = log.lock().unwrap();
        assert!(
            l.connections >= 3,
            "expected reconnects, got {} connections",
            l.connections
        );
        let frames = check_connections(&l);
        assert!(!frames.is_empty());
        // No frame ever reached the destination less than the 3 s delay after capture.
        for m in l.media.iter().filter(|m| m.kind == MediaKind::Video) {
            if let Some(id) = frame_of(&m.payload) {
                let age = m.at.saturating_duration_since(p.captured_at(id));
                assert!(
                    age >= Duration::from_millis(3_000),
                    "frame {id} aired after only {age:?}"
                );
            }
        }
        // Content resumes after every fault: the last frames are recent.
        let newest = *frames.iter().max().unwrap();
        assert!(
            newest + 250 > p.frame,
            "stream stalled: newest {newest} of {}",
            p.frame
        );
        // Reconnects resume from buffered content: at most what was in flight is lost.
        let mut seen = frames.clone();
        seen.sort_unstable();
        seen.dedup();
        let missing = (seen[0]..=newest)
            .filter(|f| seen.binary_search(f).is_err())
            .count();
        eprintln!(
            "connections {}, frames aired {}, newest {newest}/{}, lost {missing}",
            l.connections,
            frames.len(),
            p.frame
        );
        assert!(missing <= 120, "{missing} frames lost across two resets");
    }
    let state = relay.state();
    assert!(state.egress.reconnects >= 2);
    assert_eq!(state.egress.status, EgressStatus::Live);
    p.stop().await;
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn encoder_crash_within_grace_continues_the_broadcast() {
    let (sink, log, _kill) = start_sink().await;
    let relay = start_relay(
        sink,
        DestinationKey::Fixed("k".into()),
        Duration::from_secs(10),
    )
    .await;
    relay.set_delay(2_000, DelayMode::Rewind).await.unwrap();
    let mut first = Publisher::connect(relay.ingest_addr(), "x").await;
    first.stream_for(Duration::from_secs(4)).await;
    first.crash();
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!relay.state().ingest.connected);

    // OBS reconnects; its timestamps start again from 0.
    let mut second = Publisher::connect(relay.ingest_addr(), "x").await;
    second.base = 100_000;
    second.stream_for(Duration::from_secs(5)).await;

    {
        let l = log.lock().unwrap();
        assert_eq!(
            l.connections, 1,
            "the destination connection must survive the encoder crash"
        );
        assert_eq!(l.unpublished, 0, "the broadcast must not end");
        let frames = check_connections(&l);
        assert!(
            frames.iter().any(|f| *f < 100_000),
            "nothing from the first session"
        );
        let second_frames: Vec<u32> = frames.iter().copied().filter(|f| *f >= 100_000).collect();
        assert!(second_frames.len() > 60, "second session barely aired");
        assert_eq!(
            second_frames[0] % 30,
            100_000 % 30,
            "new session must start on a keyframe"
        );
    }
    second.stop().await;
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn encoder_gone_past_grace_ends_the_broadcast() {
    let (sink, log, _kill) = start_sink().await;
    let relay = start_relay(
        sink,
        DestinationKey::Fixed("k".into()),
        Duration::from_secs(1),
    )
    .await;
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_for(Duration::from_secs(2)).await;
    p.crash();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(
        log.lock().unwrap().unpublished,
        1,
        "the broadcast should end cleanly"
    );
    let state = relay.state();
    assert_eq!(state.egress.status, EgressStatus::Idle);
    assert_eq!(state.delay.phase, Phase::Offline);
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_destination_is_bounded_and_end_stream_does_not_wait_for_it() {
    let (sink, log, _kill) = start_sink().await;
    let proxy = FaultProxy::start(sink).await;
    let relay = start_relay(
        proxy.addr,
        DestinationKey::Fixed("k".into()),
        Duration::from_secs(5),
    )
    .await;
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_sized(Duration::from_secs(1), 50_000).await;
    // About 48 Mbps while nothing gets through: far more than socket buffers hold.
    proxy.stall(true);
    let mut max_backlog = 0;
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        p.stream_sized(Duration::from_millis(250), 200_000).await;
        max_backlog = max_backlog.max(relay.state().egress.backlog_bytes);
    }
    eprintln!("max backlog while stalled: {max_backlog} bytes");
    assert!(
        max_backlog > 4_000_000,
        "the stall never backed up ({max_backlog} bytes)"
    );
    // The queue stops at its limit (plus one message); the rest waits in the
    // delay buffer.
    assert!(
        max_backlog <= 9 * 1024 * 1024,
        "egress backlog kept growing: {max_backlog} bytes"
    );

    // Ending the stream must not wait for the stalled destination.
    relay.end_stream().await.unwrap();
    let ended = Instant::now();
    while relay.state().egress.status != EgressStatus::Idle
        && ended.elapsed() < Duration::from_secs(10)
    {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        relay.state().egress.status,
        EgressStatus::Idle,
        "egress stuck on the stalled destination"
    );
    assert!(
        ended.elapsed() < Duration::from_secs(1),
        "ending took {:?}",
        ended.elapsed()
    );
    assert_eq!(relay.state().egress.backlog_bytes, 0, "queue not dropped");

    // Once the destination works again, the relay can broadcast again.
    proxy.stall(false);
    relay.resume().await.unwrap();
    p.stream_sized(Duration::from_secs(4), 50_000).await;
    {
        let l = log.lock().unwrap();
        assert_eq!(l.connections, 2, "no new broadcast after resuming");
        assert!(
            l.media.iter().any(|m| m.conn == 1),
            "nothing aired after resuming"
        );
    }
    p.stop().await;
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_destination_shows_backlog_then_recovers() {
    let (sink, log, _kill) = start_sink().await;
    let proxy = FaultProxy::start(sink).await;
    let relay = start_relay(
        proxy.addr,
        DestinationKey::Fixed("k".into()),
        Duration::from_secs(5),
    )
    .await;
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    // About 12 Mbps of video, enough to fill socket buffers while stalled.
    p.stream_sized(Duration::from_secs(2), 50_000).await;
    proxy.stall(true);
    let mut max_backlog = 0;
    let end = Instant::now() + Duration::from_secs(4);
    while Instant::now() < end {
        p.stream_sized(Duration::from_millis(250), 50_000).await;
        max_backlog = max_backlog.max(relay.state().egress.backlog_bytes);
    }
    proxy.stall(false);
    p.stream_sized(Duration::from_secs(4), 50_000).await;
    eprintln!(
        "max backlog {max_backlog} bytes, after recovery {}",
        relay.state().egress.backlog_bytes
    );
    assert!(
        max_backlog > 1_000_000,
        "backlog never showed up ({max_backlog} bytes)"
    );
    assert!(
        relay.state().egress.backlog_bytes < max_backlog / 2,
        "backlog did not drain"
    );
    check_connections(&log.lock().unwrap());
    p.stop().await;
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_destination_connection_that_goes_silent_is_replaced() {
    // The network path dies without either side hearing about it (no reset, no
    // close): only a watchdog notices.
    let (sink, log, _kill) = start_sink().await;
    let proxy = FaultProxy::start(sink).await;
    let mut config = relay_config(
        proxy.addr,
        DestinationKey::Fixed("k".into()),
        Duration::from_secs(10),
    );
    config.stall_timeout = Duration::from_secs(1);
    let relay = start_relay_with(config).await;
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_sized(Duration::from_secs(1), 50_000).await;
    proxy.stall(true);
    // Enough data to fill every socket buffer on the way, so writes block.
    let stalled = Instant::now();
    while relay.state().egress.reconnects == 0 && stalled.elapsed() < Duration::from_secs(8) {
        p.stream_sized(Duration::from_millis(250), 200_000).await;
    }
    let state = relay.state();
    assert_eq!(state.egress.reconnects, 1, "the dead connection was kept");
    assert!(
        state
            .egress
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("took no data")),
        "{:?}",
        state.egress.last_error
    );
    // The network comes back: the stream continues on a new connection.
    proxy.stall(false);
    proxy.reset_all();
    p.stream_sized(Duration::from_secs(4), 50_000).await;
    {
        let l = log.lock().unwrap();
        assert!(
            l.media
                .iter()
                .any(|m| m.conn >= 1 && m.kind == MediaKind::Video),
            "nothing aired after the stall"
        );
        check_connections(&l);
    }
    p.stop().await;
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_destination_connection_is_retried_at_once() {
    let (sink, log, _kill) = start_sink().await;
    let proxy = FaultProxy::start(sink).await;
    let relay = start_relay(
        proxy.addr,
        DestinationKey::Fixed("k".into()),
        Duration::from_secs(10),
    )
    .await;
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    // Long enough for the connection to count as working.
    p.stream_for(Duration::from_secs(11)).await;
    let dropped = Instant::now();
    proxy.reset_all();
    p.stream_for(Duration::from_secs(2)).await;
    let back = log
        .lock()
        .unwrap()
        .media
        .iter()
        .find(|m| m.conn == 1)
        .map(|m| m.at.duration_since(dropped));
    // Every second spent reconnecting would be a second more delay.
    assert!(
        back.is_some_and(|b| b < Duration::from_millis(500)),
        "back after {back:?}"
    );
    p.stop().await;
    relay.shutdown().await;
}

/// How the upload to the destination has stalled when a dump comes.
#[derive(Debug, Clone, Copy)]
enum Stall {
    /// Megabytes are queued for it in the relay.
    Flooded,
    /// A write of a frame or two is blocked: the queue is far below what the
    /// relay once took to mean "behind".
    Trickling,
    /// The encoder paused after the stall: nothing is queued in the relay, but
    /// the OS still holds what it could not send.
    Paused,
}

/// A dump must keep everything recorded before it that viewers have not seen
/// from reaching them, including what is already on its way to a destination
/// that has fallen behind (it had been emitted, so the delay buffer no longer
/// held it): queued in the relay, in a write, or unsent in the OS. Only what
/// had reached the destination is out of reach: here, what the proxy had
/// received from the relay when the dump came, forwarded or waiting in its
/// receive buffer. Nothing else from before the dump may arrive, not one byte.
async fn dump_on_a_stalled_upload(mode: DelayMode, stall: Stall) {
    let (sink, log, _kill) = start_sink().await;
    let proxy = FaultProxy::start_with(sink, Some(64 * 1024)).await;
    let relay = start_relay(
        proxy.addr,
        DestinationKey::Fixed("k".into()),
        Duration::from_secs(5),
    )
    .await;
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    relay.set_delay(1000, DelayMode::Mask).await.unwrap();
    p.stream_for(Duration::from_secs(3)).await;
    proxy.stall(true);
    let deadline = Instant::now() + Duration::from_secs(30);
    match stall {
        Stall::Flooded => {
            // Far more than the upload takes: the queue for the destination fills.
            while relay.state().egress.backlog_bytes < 4_000_000 {
                assert!(Instant::now() < deadline, "the queue never filled");
                p.stream_sized(Duration::from_millis(100), 128 * 1024).await;
            }
        }
        Stall::Trickling => {
            // About 4 Mbit/s: a write blocks once the OS buffers are full (up to
            // 4 MB on Linux), with a frame or two queued.
            while relay.state().egress.backlog_bytes == 0 {
                assert!(Instant::now() < deadline, "no write ever blocked");
                p.stream_sized(Duration::from_millis(50), 16_000).await;
            }
        }
        Stall::Paused => {
            // Until the proxy takes no more, and a little beyond.
            let (_, up) = proxy.current();
            let mut last = 0;
            loop {
                assert!(Instant::now() < deadline, "the proxy kept taking data");
                p.stream_sized(Duration::from_millis(300), 4_000).await;
                let now = up.arrived();
                if now == last {
                    break;
                }
                last = now;
            }
            p.stream_sized(Duration::from_millis(150), 4_000).await;
            // What was recorded is emitted a delay later.
            tokio::time::sleep(Duration::from_millis(1_500)).await;
        }
    }
    let (conn, settled) = proxy.settled().await;
    let backlog = relay.state().egress.backlog_bytes;
    eprintln!("{stall:?}: {backlog} bytes queued in the relay, {settled} received by the proxy");
    match stall {
        Stall::Flooded => {}
        Stall::Trickling => assert!(backlog < 512 * 1024, "{backlog}"),
        Stall::Paused => assert_eq!(backlog, 0),
    }
    let before: std::collections::HashSet<u32> = video_frames(&log.lock().unwrap())
        .into_iter()
        .map(|(_, f, _)| f)
        .collect();
    let last_before_dump = p.frame - 1;
    relay.dump(mode).await.unwrap();
    let dumped = Instant::now();
    // What had reached the destination when the dump was answered, forwarded or
    // waiting in the proxy's receive buffer (as its next peeks see it). Some may
    // have come since it settled, as the proxy's receive window grew.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let up = proxy.conns.lock().unwrap()[conn].clone();
    let arrived = up.arrived();
    up.pause_at.store(arrived, Ordering::SeqCst);
    proxy.stall(false);
    p.stream_for(Duration::from_secs(5)).await;
    // If the OS still held some, the connection was reset (and made again):
    // then not one more byte of it arrived, not even part of a frame.
    let reset = proxy.conns.lock().unwrap().len() > conn + 1;
    let carried = up.forwarded.load(Ordering::SeqCst);
    eprintln!("reset: {reset}; {arrived} bytes had arrived at the dump, {carried} in all");
    if reset {
        assert!(
            carried <= arrived,
            "{} bytes arrived after the dump on the connection it reset",
            carried - arrived
        );
    }

    {
        let l = log.lock().unwrap();
        // What had reached the destination, and may air again in a replay.
        let reached: std::collections::HashSet<u32> = l
            .media
            .iter()
            .filter(|m| m.at <= dumped || (m.conn == conn && m.end <= arrived))
            .filter_map(|m| frame_of(&m.payload))
            .collect();
        let leaked: Vec<(u32, usize, u64)> = l
            .media
            .iter()
            .filter(|m| m.kind == MediaKind::Video && m.at > dumped)
            .filter_map(|m| Some((frame_of(&m.payload)?, m.conn, m.end)))
            .filter(|&(f, ..)| f <= last_before_dump && !reached.contains(&f))
            .collect();
        assert!(
            leaked.is_empty(),
            "recorded before the dump, aired after it (frame, connection, end): {leaked:?}"
        );
        let late = reached.len() - before.len();
        eprintln!("{late} frames from before the dump were already at the destination");
        assert!(
            video_frames(&l)
                .iter()
                .any(|&(_, f, _)| f > last_before_dump),
            "the stream did not go on after the dump"
        );
        check_connections(&l);
    }
    p.stop().await;
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mask_dump_on_a_flooded_upload_airs_nothing_from_before_it() {
    dump_on_a_stalled_upload(DelayMode::Mask, Stall::Flooded).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rewind_dump_on_a_flooded_upload_airs_nothing_new_from_before_it() {
    dump_on_a_stalled_upload(DelayMode::Rewind, Stall::Flooded).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mask_dump_on_a_trickling_stalled_upload_airs_nothing_from_before_it() {
    dump_on_a_stalled_upload(DelayMode::Mask, Stall::Trickling).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rewind_dump_on_a_trickling_stalled_upload_airs_nothing_new_from_before_it() {
    dump_on_a_stalled_upload(DelayMode::Rewind, Stall::Trickling).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mask_dump_after_the_encoder_paused_on_a_stalled_upload_airs_nothing_from_before_it() {
    dump_on_a_stalled_upload(DelayMode::Mask, Stall::Paused).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rewind_dump_after_the_encoder_paused_on_a_stalled_upload_airs_nothing_new_from_before_it()
 {
    dump_on_a_stalled_upload(DelayMode::Rewind, Stall::Paused).await;
}
