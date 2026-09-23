//! RTMP ingest server: accepts the encoder's publish connection.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use streamdelay_engine::Kind;
use streamdelay_rtmp::amf0::Amf0Value;
use streamdelay_rtmp::session::{MediaKind, ServerConfig, ServerEvent, ServerSession};
use streamdelay_rtmp::ts::TsUnwrapper;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc, oneshot, watch};
use tracing::{debug, info, warn};

use crate::core::Event;
use crate::io;

/// If the encoder sends nothing for this long, the connection is considered dead.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Connections handled at once. Only one can publish; the rest are waiting to be
/// rejected or are stale, so this only needs headroom for encoder reconnects.
const MAX_CONNECTIONS: usize = 16;

/// `connect` properties we set ourselves on the egress side.
const OWN_CONNECT_PROPS: &[&str] = &[
    "app",
    "type",
    "flashVer",
    "swfUrl",
    "tcUrl",
    "pageUrl",
    "objectEncoding",
];

pub(crate) async fn listen(
    listener: TcpListener,
    events: mpsc::UnboundedSender<Event>,
    mut shutdown: watch::Receiver<bool>,
) {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let accepted = tokio::select! {
            a = listener.accept() => a,
            _ = shutdown.changed() => return,
        };
        match accepted {
            Ok((tcp, peer)) => {
                let Ok(slot) = slots.clone().try_acquire_owned() else {
                    warn!(%peer, "too many ingest connections; refusing");
                    continue;
                };
                let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                let events = events.clone();
                let mut shutdown = shutdown.clone();
                tokio::spawn(async move {
                    let result = tokio::select! {
                        r = handle(tcp, peer, id, events.clone()) => r,
                        _ = shutdown.changed() => Ok(()),
                    };
                    if let Err(e) = result {
                        debug!(%peer, "ingest connection ended: {e}");
                    }
                    let _ = events.send(Event::IngestClosed { conn: id });
                    drop(slot);
                });
            }
            Err(e) => {
                warn!("ingest accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

async fn handle(
    mut tcp: TcpStream,
    peer: SocketAddr,
    conn: u64,
    events: mpsc::UnboundedSender<Event>,
) -> std::io::Result<()> {
    io::tune(&tcp);
    let rest = tokio::time::timeout(IDLE_TIMEOUT, io::server_handshake(&mut tcp))
        .await
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))??;
    let mut session = ServerSession::new(ServerConfig::default());
    let mut connect_props: Vec<(String, Amf0Value)> = Vec::new();
    let mut unwrap = [TsUnwrapper::new(), TsUnwrapper::new(), TsUnwrapper::new()];
    let mut publishing = false;
    let mut buf = vec![0u8; 64 * 1024];
    let mut pending: Option<Bytes> = Some(rest);

    loop {
        let data = match pending.take() {
            Some(d) => d,
            None => {
                let n = tokio::time::timeout(IDLE_TIMEOUT, tcp.read(&mut buf))
                    .await
                    .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))??;
                if n == 0 {
                    return Ok(());
                }
                Bytes::copy_from_slice(&buf[..n])
            }
        };
        let evs = session
            .feed(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        for ev in evs {
            match ev {
                ServerEvent::Connect { props, .. } => {
                    connect_props = props
                        .into_iter()
                        .filter(|(k, _)| !OWN_CONNECT_PROPS.contains(&k.as_str()))
                        .collect();
                }
                ServerEvent::PublishRequest { app, stream_key } => {
                    let (tx, rx) = oneshot::channel();
                    let _ = events.send(Event::IngestPublish {
                        conn,
                        peer,
                        app,
                        key: stream_key,
                        connect_props: connect_props.clone(),
                        reply: tx,
                    });
                    match rx.await {
                        Ok(Ok(())) => {
                            info!(%peer, "encoder started publishing");
                            session.accept_publish();
                            publishing = true;
                        }
                        Ok(Err(reason)) => {
                            warn!(%peer, "rejected publish: {reason}");
                            session.reject_publish("NetStream.Publish.BadName", &reason);
                            tcp.write_all(&session.take_output()).await?;
                            return Ok(());
                        }
                        Err(_) => return Ok(()),
                    }
                }
                ServerEvent::Media {
                    kind,
                    timestamp,
                    payload,
                } => {
                    let (k, i) = match kind {
                        MediaKind::Audio => (Kind::Audio, 0),
                        MediaKind::Video => (Kind::Video, 1),
                    };
                    let ts = unwrap[i].unwrap(timestamp);
                    let _ = events.send(Event::IngestMedia {
                        conn,
                        kind: k,
                        ts,
                        payload,
                    });
                }
                ServerEvent::Data { timestamp, payload } => {
                    let ts = unwrap[2].unwrap(timestamp);
                    let _ = events.send(Event::IngestMedia {
                        conn,
                        kind: Kind::Data,
                        ts,
                        payload,
                    });
                }
                ServerEvent::Metadata { payload, .. } => {
                    let _ = events.send(Event::IngestMetadata { conn, payload });
                }
                ServerEvent::Unpublish => {
                    if publishing {
                        info!(%peer, "encoder stopped publishing");
                        publishing = false;
                        let _ = events.send(Event::IngestClosed { conn });
                    }
                }
            }
        }
        let out = session.take_output();
        if !out.is_empty() {
            tcp.write_all(&out).await?;
        }
    }
}
