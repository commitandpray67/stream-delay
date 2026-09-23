//! Shared test helpers: an in-process RTMP sink standing in for Twitch, and a
//! synthetic publisher standing in for OBS.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::{BufMut, Bytes, BytesMut};
use streamdelay_relay::{Destination, DestinationKey, EngineConfig, RelayConfig, RelayHandle};
use streamdelay_rtmp::handshake::{ClientHandshake, Progress, ServerHandshake};
use streamdelay_rtmp::session::{
    ClientConfig, ClientEvent, ClientSession, MediaKind, ServerConfig, ServerEvent, ServerSession,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Debug, Clone)]
pub struct Received {
    /// Index of the sink connection (0-based) this arrived on.
    pub conn: usize,
    pub at: Instant,
    pub kind: MediaKind,
    pub ts: u32,
    pub payload: Bytes,
}

#[derive(Default)]
pub struct SinkLog {
    pub keys: Vec<String>,
    pub media: Vec<Received>,
    pub metadata: usize,
    pub connections: usize,
    pub unpublished: usize,
}

/// A minimal RTMP server standing in for Twitch.
pub async fn start_sink() -> (
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
            let Ok((mut tcp, _)) = listener.accept().await else {
                return;
            };
            let conn = {
                let mut l = log2.lock().unwrap();
                l.connections += 1;
                l.connections - 1
            };
            let log = log2.clone();
            let mut kill = kill_rx.clone();
            kill.mark_unchanged();
            tokio::spawn(async move {
                let mut hs = ServerHandshake::new();
                let mut out = BytesMut::new();
                let mut buf = vec![0u8; 65536];
                let rest = loop {
                    let Ok(n) = tcp.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    let p = hs.feed(&buf[..n], &mut out).unwrap();
                    if tcp.write_all(&out.split()).await.is_err() {
                        return;
                    }
                    if let Progress::Done(rest) = p {
                        break rest;
                    }
                };
                let mut s = ServerSession::new(ServerConfig::default());
                let mut data = rest.to_vec();
                loop {
                    let Ok(events) = s.feed(&data) else { return };
                    for ev in events {
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
                                conn,
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
pub struct Publisher {
    tcp: TcpStream,
    session: ClientSession,
    pub frame: u32,
    audio: u32,
    pub started: Instant,
    /// Added to frame numbers in payloads, to tell encoder sessions apart.
    pub base: u32,
}

pub fn video_payload(frame: u32) -> Bytes {
    video_payload_sized(frame, 0, 400)
}

/// Video payload for frame `frame` (keyframe every 30), carrying id `base + frame`.
pub fn video_payload_sized(frame: u32, base: u32, size: usize) -> Bytes {
    let mut b = BytesMut::new();
    let key = frame.is_multiple_of(30);
    b.put_slice(if key {
        &[0x17, 0x01, 0, 0, 0]
    } else {
        &[0x27, 0x01, 0, 0, 0]
    });
    b.put_u32(base + frame);
    b.put_bytes(0xab, size);
    b.freeze()
}

pub fn frame_of(p: &[u8]) -> Option<u32> {
    if p.len() < 9 || p[1] != 0x01 || (p[0] != 0x17 && p[0] != 0x27) {
        return None;
    }
    Some(u32::from_be_bytes([p[5], p[6], p[7], p[8]]))
}

impl Publisher {
    pub async fn connect(addr: SocketAddr, key: &str) -> Self {
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
            base: 0,
        }
    }

    /// Streams in real time for `d`.
    pub async fn stream_for(&mut self, d: Duration) {
        self.stream_sized(d, 400).await;
    }

    /// Streams in real time with video frames of `size` bytes.
    pub async fn stream_sized(&mut self, d: Duration, size: usize) {
        let end = Instant::now() + d;
        while Instant::now() < end {
            let elapsed = self.started.elapsed().as_millis() as u32;
            while self.frame * 33 <= elapsed {
                self.session.send_media(
                    MediaKind::Video,
                    self.frame * 33,
                    &video_payload_sized(self.frame, self.base, size),
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

    /// When frame `id` (including `base`) was captured.
    pub fn captured_at(&self, id: u32) -> Instant {
        self.started + Duration::from_millis(u64::from(id - self.base) * 33)
    }

    /// Drops the connection without unpublishing, like a crashed encoder.
    pub fn crash(self) {
        // Linger 0 makes the close an abortive reset, like a process crash.
        let _ = socket2::SockRef::from(&self.tcp).set_linger(Some(Duration::ZERO));
        drop(self.tcp);
    }

    pub async fn stop(mut self) {
        self.session.close();
        let o = self.session.take_output();
        let _ = self.tcp.write_all(&o).await;
        let _ = self.tcp.shutdown().await;
    }
}

pub async fn start_relay(sink: SocketAddr, key: DestinationKey, grace: Duration) -> RelayHandle {
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

pub fn video_frames(log: &SinkLog) -> Vec<(Instant, u32, u32)> {
    log.media
        .iter()
        .filter(|m| m.kind == MediaKind::Video)
        .filter_map(|m| frame_of(&m.payload).map(|f| (m.at, f, m.ts)))
        .collect()
}

pub fn assert_monotonic(log: &SinkLog) {
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
