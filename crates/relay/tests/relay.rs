//! End-to-end tests over real sockets: a synthetic publisher streams through the relay
//! to an in-process RTMP sink.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::{BufMut, Bytes, BytesMut};
use streamdelay_relay::{
    DelayMode, Destination, DestinationKey, EgressStatus, EngineConfig, GoLiveWhen, RelayConfig,
    RelayHandle,
};
use streamdelay_rtmp::handshake::{ClientHandshake, Progress, ServerHandshake};
use streamdelay_rtmp::session::{
    ClientConfig, ClientEvent, ClientSession, MediaKind, ServerConfig, ServerEvent, ServerSession,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Debug, Clone)]
struct Received {
    at: Instant,
    kind: MediaKind,
    ts: u32,
    payload: Bytes,
}

#[derive(Default)]
struct SinkLog {
    keys: Vec<String>,
    media: Vec<Received>,
    metadata: usize,
    connections: usize,
    unpublished: usize,
}

/// A minimal RTMP server standing in for Twitch.
async fn start_sink() -> (
    SocketAddr,
    Arc<Mutex<SinkLog>>,
    tokio::sync::watch::Sender<bool>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log = Arc::new(Mutex::new(SinkLog::default()));
    // Sending `true` drops the current connection (to simulate a network failure).
    let (kill_tx, kill_rx) = tokio::sync::watch::channel(false);
    let log2 = log.clone();
    tokio::spawn(async move {
        loop {
            let (mut tcp, _) = listener.accept().await.unwrap();
            log2.lock().unwrap().connections += 1;
            let log = log2.clone();
            let mut kill = kill_rx.clone();
            kill.mark_unchanged();
            tokio::spawn(async move {
                let mut hs = ServerHandshake::new();
                let mut out = BytesMut::new();
                let mut buf = vec![0u8; 65536];
                let rest = loop {
                    let n = tcp.read(&mut buf).await.unwrap();
                    let p = hs.feed(&buf[..n], &mut out).unwrap();
                    tcp.write_all(&out.split()).await.unwrap();
                    if let Progress::Done(rest) = p {
                        break rest;
                    }
                };
                let mut s = ServerSession::new(ServerConfig::default());
                let mut data = rest.to_vec();
                loop {
                    for ev in s.feed(&data).unwrap() {
                        let mut l = log.lock().unwrap();
                        match ev {
                            ServerEvent::PublishRequest { stream_key, .. } => {
                                l.keys.push(stream_key);
                                s.accept_publish();
                            }
                            ServerEvent::Media {
                                kind,
                                timestamp,
                                payload,
                            } => l.media.push(Received {
                                at: Instant::now(),
                                kind,
                                ts: timestamp,
                                payload,
                            }),
                            ServerEvent::Metadata { .. } => l.metadata += 1,
                            ServerEvent::Unpublish => l.unpublished += 1,
                            _ => {}
                        }
                    }
                    let o = s.take_output();
                    if !o.is_empty() && tcp.write_all(&o).await.is_err() {
                        return;
                    }
                    tokio::select! {
                        n = tcp.read(&mut buf) => match n {
                            Ok(0) | Err(_) => return,
                            Ok(n) => data = buf[..n].to_vec(),
                        },
                        _ = kill.changed() => return,
                    }
                }
            });
        }
    });
    (addr, log, kill_tx)
}

/// Synthetic encoder: 30 fps, keyframe every 30 frames (1 s), AAC every ~21 ms.
struct Publisher {
    tcp: TcpStream,
    session: ClientSession,
    frame: u32,
    audio: u32,
    started: Instant,
}

fn video_payload(frame: u32) -> Bytes {
    let mut b = BytesMut::new();
    let key = frame % 30 == 0;
    b.put_slice(if key {
        &[0x17, 0x01, 0, 0, 0]
    } else {
        &[0x27, 0x01, 0, 0, 0]
    });
    b.put_u32(frame);
    b.put_slice(&[0xab; 400]);
    b.freeze()
}

fn frame_of(p: &[u8]) -> Option<u32> {
    if p.len() < 9 || p[1] != 0x01 || (p[0] != 0x17 && p[0] != 0x27) {
        return None;
    }
    Some(u32::from_be_bytes([p[5], p[6], p[7], p[8]]))
}

impl Publisher {
    async fn connect(addr: SocketAddr, key: &str) -> Self {
        let mut tcp = TcpStream::connect(addr).await.unwrap();
        let mut hs = ClientHandshake::new();
        let mut out = BytesMut::new();
        hs.start(&mut out);
        tcp.write_all(&out.split()).await.unwrap();
        let mut buf = vec![0u8; 65536];
        let rest = loop {
            let n = tcp.read(&mut buf).await.unwrap();
            if let Progress::Done(rest) = hs.feed(&buf[..n], &mut out).unwrap() {
                tcp.write_all(&out.split()).await.unwrap();
                break rest;
            }
        };
        let mut session = ClientSession::new(ClientConfig::new(
            "live",
            format!("rtmp://{addr}/live"),
            key,
        ));
        let mut data = rest.to_vec();
        loop {
            let evs = session.feed(&data).unwrap();
            let o = session.take_output();
            tcp.write_all(&o).await.unwrap();
            if evs.contains(&ClientEvent::Publishing) {
                break;
            }
            if let Some(ClientEvent::Error { code, .. }) = evs.first() {
                panic!("publish rejected: {code}");
            }
            let n = tcp.read(&mut buf).await.unwrap();
            assert!(n > 0, "relay closed the connection");
            data = buf[..n].to_vec();
        }
        let meta = streamdelay_rtmp::amf0::encode_all(&[
            streamdelay_rtmp::amf0::Amf0Value::string("@setDataFrame"),
            streamdelay_rtmp::amf0::Amf0Value::string("onMetaData"),
            streamdelay_rtmp::amf0::Amf0Value::EcmaArray(vec![]),
        ]);
        session.send_data(0, &meta);
        session.send_media(
            MediaKind::Video,
            0,
            &[0x17, 0x00, 0, 0, 0, 1, 0x64, 0, 0x1f],
        );
        session.send_media(MediaKind::Audio, 0, &[0xaf, 0x00, 0x11, 0x90]);
        let o = session.take_output();
        tcp.write_all(&o).await.unwrap();
        Self {
            tcp,
            session,
            frame: 0,
            audio: 0,
            started: Instant::now(),
        }
    }

    /// Streams in real time for `d`.
    async fn stream_for(&mut self, d: Duration) {
        let end = Instant::now() + d;
        while Instant::now() < end {
            let elapsed = self.started.elapsed().as_millis() as u32;
            while self.frame * 33 <= elapsed {
                self.session.send_media(
                    MediaKind::Video,
                    self.frame * 33,
                    &video_payload(self.frame),
                );
                self.frame += 1;
            }
            while self.audio * 21 <= elapsed {
                self.session.send_media(
                    MediaKind::Audio,
                    self.audio * 21,
                    &[0xaf, 0x01, 0x21, 0x00],
                );
                self.audio += 1;
            }
            let o = self.session.take_output();
            self.tcp.write_all(&o).await.unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn stop(mut self) {
        self.session.close();
        let o = self.session.take_output();
        let _ = self.tcp.write_all(&o).await;
        let _ = self.tcp.shutdown().await;
    }
}

async fn start_relay(sink: SocketAddr, key: DestinationKey, grace: Duration) -> RelayHandle {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    streamdelay_relay::start(RelayConfig {
        ingest_bind: "127.0.0.1:0".parse().unwrap(),
        destination: Some(Destination {
            url: format!("rtmp://{sink}/app"),
            key,
        }),
        engine: EngineConfig {
            max_delay_ms: 30_000,
            ..Default::default()
        },
        encoder_grace: grace,
        ..Default::default()
    })
    .await
    .unwrap()
}

fn video_frames(log: &SinkLog) -> Vec<(Instant, u32, u32)> {
    log.media
        .iter()
        .filter(|m| m.kind == MediaKind::Video)
        .filter_map(|m| frame_of(&m.payload).map(|f| (m.at, f, m.ts)))
        .collect()
}

fn assert_monotonic(log: &SinkLog) {
    for kind in [MediaKind::Audio, MediaKind::Video] {
        let ts: Vec<u32> = log
            .media
            .iter()
            .filter(|m| m.kind == kind)
            .map(|m| m.ts)
            .collect();
        assert!(
            ts.windows(2).all(|w| w[0] <= w[1]),
            "{kind:?} timestamps went backwards"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn passthrough_is_byte_faithful_and_immediate() {
    let (sink, log, _kill) = start_sink().await;
    let relay = start_relay(
        sink,
        DestinationKey::Fixed("twitch-key".into()),
        Duration::from_secs(1),
    )
    .await;
    let mut p = Publisher::connect(relay.ingest_addr(), "anything").await;
    p.stream_for(Duration::from_secs(3)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    {
        let l = log.lock().unwrap();
        assert_eq!(l.keys, vec!["twitch-key".to_string()]);
        assert_eq!(l.metadata, 1);
        // Decoder configuration first, then frames in order and unmodified.
        assert_eq!(&l.media[0].payload[..2], &[0x17, 0x00]);
        let frames = video_frames(&l);
        assert!(frames.len() > 60, "only {} frames", frames.len());
        assert!(
            frames.windows(2).all(|w| w[1].1 == w[0].1 + 1),
            "frames skipped or repeated"
        );
        for m in l.media.iter().filter(|m| frame_of(&m.payload).is_some()) {
            assert_eq!(m.payload, video_payload(frame_of(&m.payload).unwrap()));
        }
        assert_monotonic(&l);
    }
    let state = relay.state();
    assert_eq!(state.egress.status, EgressStatus::Live);
    assert!(state.ingest.connected);
    p.stop().await;
    // The broadcast ends after the grace period.
    tokio::time::sleep(Duration::from_millis(1800)).await;
    assert_eq!(log.lock().unwrap().unpublished, 1);
    assert_eq!(relay.state().egress.status, EgressStatus::Idle);
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delay_rewind_and_go_live() {
    let (sink, log, _kill) = start_sink().await;
    let relay = start_relay(
        sink,
        DestinationKey::Fixed("k".into()),
        Duration::from_secs(5),
    )
    .await;
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_for(Duration::from_secs(4)).await;

    let ack = relay.set_delay(2_000, DelayMode::Rewind).await.unwrap();
    assert!((2_000..=3_100).contains(&ack.effective_ms), "{ack:?}");
    let t_rewind = Instant::now();
    p.stream_for(Duration::from_secs(3)).await;
    {
        let l = log.lock().unwrap();
        let frames = video_frames(&l);
        let after: Vec<_> = frames.iter().filter(|(at, _, _)| *at >= t_rewind).collect();
        let before_max = frames
            .iter()
            .filter(|(at, _, _)| *at < t_rewind)
            .map(|f| f.1)
            .max()
            .unwrap();
        // Viewers see some frames again, starting from a keyframe.
        assert!(after[0].1 <= before_max, "no rewind happened");
        assert_eq!(after[0].1 % 30, 0, "rewind did not land on a keyframe");
        assert_monotonic(&l);
    }
    assert_eq!(relay.state().delay.phase, streamdelay_relay::Phase::Delayed);

    relay.go_live(GoLiveWhen::Now).await.unwrap();
    p.stream_for(Duration::from_millis(1500)).await;
    let state = relay.state();
    assert_eq!(
        state.delay.phase,
        streamdelay_relay::Phase::Live,
        "{:?}",
        state.delay
    );
    assert!(state.delay.effective_ms < 200);
    assert_monotonic(&log.lock().unwrap());
    p.stop().await;
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn destination_reconnects_after_a_drop() {
    let (sink, log, kill) = start_sink().await;
    let relay = start_relay(
        sink,
        DestinationKey::Fixed("k".into()),
        Duration::from_secs(5),
    )
    .await;
    relay.set_delay(3_000, DelayMode::Rewind).await.unwrap();
    let mut p = Publisher::connect(relay.ingest_addr(), "x").await;
    p.stream_for(Duration::from_secs(5)).await;
    kill.send(true).unwrap();
    p.stream_for(Duration::from_secs(4)).await;
    {
        let l = log.lock().unwrap();
        assert!(l.connections >= 2, "relay did not reconnect");
        assert_eq!(l.keys.len(), l.connections);
        // Media continued after the reconnect.
        let frames = video_frames(&l);
        let last = frames.last().unwrap().1;
        assert!(last > 100, "stream stalled at frame {last}");
    }
    assert!(relay.state().egress.reconnects >= 1);
    p.stop().await;
    relay.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn passthrough_key_and_second_publisher_rejected() {
    let (sink, log, _kill) = start_sink().await;
    let relay = start_relay(sink, DestinationKey::Passthrough, Duration::from_secs(1)).await;
    let mut p = Publisher::connect(relay.ingest_addr(), "live_123?bandwidthtest=true").await;
    p.stream_for(Duration::from_secs(1)).await;
    // A second encoder is refused while the first is live.
    let second = tokio::spawn(Publisher::connect(relay.ingest_addr(), "other"));
    let result = second.await;
    assert!(
        result.is_err_and(|e| e.is_panic()),
        "second publisher was accepted"
    );
    p.stream_for(Duration::from_millis(500)).await;
    assert_eq!(
        log.lock().unwrap().keys,
        vec!["live_123?bandwidthtest=true".to_string()]
    );
    p.stop().await;
    relay.shutdown().await;
}
