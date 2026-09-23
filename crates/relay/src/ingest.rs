//! RTMP ingest server: accepts the encoder's publish connection.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use streamdelay_engine::Kind;
use streamdelay_rtmp::amf0::Amf0Value;
use streamdelay_rtmp::session::{MediaKind, ServerConfig, ServerEvent, ServerSession};
use streamdelay_rtmp::ts::TsUnwrapper;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc, oneshot, watch};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::core::Event;
use crate::io;

/// If the encoder sends nothing for this long, the connection is considered dead.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a refused publish (for example a wrong ingest key) waits for its answer.
const REJECT_DELAY: Duration = Duration::from_secs(1);

/// Connections handled at once. Only one can publish; the rest are waiting to be
/// rejected or are stale, so this only needs headroom for encoder reconnects.
const MAX_CONNECTIONS: usize = 16;

/// Connections from one non-loopback address at once, so a single host on the
/// network cannot take every slot.
const MAX_CONNECTIONS_PER_IP: usize = 4;

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

/// `publish_timeout`: a connection that is not publishing must start within this
/// time of connecting (or of stopping), handshake included. Encoders publish within
/// a second or two; anything else would only hold a slot.
pub(crate) async fn listen(
    listener: TcpListener,
    publish_timeout: Duration,
    events: mpsc::UnboundedSender<Event>,
    mut shutdown: watch::Receiver<bool>,
) {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let per_ip = PerIp::default();
    loop {
        let accepted = tokio::select! {
            a = listener.accept() => a,
            _ = shutdown.changed() => return,
        };
        match accepted {
            Ok((tcp, peer)) => {
                let Some(ip_slot) = per_ip.acquire(peer.ip().to_canonical()) else {
                    warn!(%peer, "too many ingest connections from this address; refusing");
                    continue;
                };
                let Ok(slot) = slots.clone().try_acquire_owned() else {
                    warn!(%peer, "too many ingest connections; refusing");
                    continue;
                };
                let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                let events = events.clone();
                let mut shutdown = shutdown.clone();
                tokio::spawn(async move {
                    info!(%peer, "encoder connected");
                    let result = tokio::select! {
                        r = handle(tcp, peer, id, publish_timeout, events.clone()) => r,
                        _ = shutdown.changed() => Ok(()),
                    };
                    let error = match result {
                        Ok(()) => {
                            debug!(%peer, "encoder connection closed");
                            None
                        }
                        Err(e) => {
                            warn!(%peer, "encoder connection failed: {e}");
                            Some(e.to_string())
                        }
                    };
                    let _ = events.send(Event::IngestClosed { conn: id, error });
                    drop(slot);
                    drop(ip_slot);
                });
            }
            Err(e) => {
                warn!("ingest accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

/// Counts connections per remote address. Loopback peers are not limited: they
/// are local programs, and the global limit still applies.
#[derive(Clone, Default)]
struct PerIp(Arc<Mutex<HashMap<IpAddr, usize>>>);

struct IpSlot {
    map: PerIp,
    /// `None` for loopback peers, which are not counted.
    ip: Option<IpAddr>,
}

impl PerIp {
    fn acquire(&self, ip: IpAddr) -> Option<IpSlot> {
        let ip = (!ip.is_loopback()).then_some(ip);
        if let Some(ip) = ip {
            let mut m = self.0.lock().ok()?;
            let n = m.entry(ip).or_insert(0);
            if *n >= MAX_CONNECTIONS_PER_IP {
                return None;
            }
            *n += 1;
        }
        Some(IpSlot {
            map: self.clone(),
            ip,
        })
    }
}

impl Drop for IpSlot {
    fn drop(&mut self) {
        if let Some(ip) = self.ip
            && let Ok(mut m) = self.map.0.lock()
            && let Some(n) = m.get_mut(&ip)
        {
            *n -= 1;
            if *n == 0 {
                m.remove(&ip);
            }
        }
    }
}

fn timed_out(what: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::TimedOut, what.to_string())
}

/// Writes with a timeout, so a peer that stops reading cannot stall the task.
async fn write(tcp: &mut TcpStream, data: &[u8]) -> std::io::Result<()> {
    tokio::time::timeout(IDLE_TIMEOUT, tcp.write_all(data))
        .await
        .map_err(|_| timed_out("the encoder stopped reading"))?
}

async fn handle(
    mut tcp: TcpStream,
    peer: SocketAddr,
    conn: u64,
    publish_timeout: Duration,
    events: mpsc::UnboundedSender<Event>,
) -> std::io::Result<()> {
    io::tune(&tcp);
    // Set while not publishing: the time by which the connection must publish.
    let mut deadline = Some(Instant::now() + publish_timeout);
    let not_publishing = || timed_out("the encoder did not start publishing in time");
    let rest = tokio::time::timeout(publish_timeout, io::server_handshake(&mut tcp))
        .await
        .map_err(|_| not_publishing())??;
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
                let idle = Instant::now() + IDLE_TIMEOUT;
                let until = deadline.map_or(idle, |d| d.min(idle));
                let n = tokio::time::timeout_at(until, tcp.read(&mut buf))
                    .await
                    .map_err(|_| {
                        if deadline.is_some_and(|d| d <= Instant::now()) {
                            not_publishing()
                        } else {
                            std::io::Error::from(std::io::ErrorKind::TimedOut)
                        }
                    })??;
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
                ServerEvent::Connect { app, props, .. } => {
                    let encoder = props
                        .iter()
                        .find(|(k, _)| k == "flashVer")
                        .and_then(|(_, v)| v.as_str())
                        .unwrap_or("unknown");
                    info!(%peer, %app, %encoder, "encoder sent connect");
                    connect_props = props
                        .into_iter()
                        .filter(|(k, _)| !OWN_CONNECT_PROPS.contains(&k.as_str()))
                        .collect();
                }
                ServerEvent::PublishRequest { app, stream_key } => {
                    info!(%peer, %app, "encoder asked to publish");
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
                            deadline = None;
                        }
                        Ok(Err(reason)) => {
                            warn!(%peer, "rejected publish: {reason}");
                            // Answer slowly, so the ingest key can't be guessed quickly:
                            // with the per-address connection limit, one address gets
                            // a handful of tries per second.
                            tokio::time::sleep(REJECT_DELAY).await;
                            session.reject_publish("NetStream.Publish.BadName", &reason);
                            write(&mut tcp, &session.take_output()).await?;
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
                        deadline = Some(Instant::now() + publish_timeout);
                        let _ = events.send(Event::IngestClosed { conn, error: None });
                    }
                }
            }
        }
        let out = session.take_output();
        if !out.is_empty() {
            write(&mut tcp, &out).await?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_ip_limit_skips_loopback_and_frees_slots() {
        let per_ip = PerIp::default();
        let lan: IpAddr = "192.0.2.7".parse().unwrap();
        let held: Vec<IpSlot> = (0..MAX_CONNECTIONS_PER_IP)
            .map(|_| per_ip.acquire(lan).unwrap())
            .collect();
        assert!(per_ip.acquire(lan).is_none(), "fifth connection allowed");
        let other = per_ip.acquire("192.0.2.8".parse().unwrap());
        assert!(other.is_some(), "limit is per address");
        // Loopback peers are never counted.
        let local: Vec<IpSlot> = (0..MAX_CONNECTIONS_PER_IP * 2)
            .map(|_| per_ip.acquire("127.0.0.1".parse().unwrap()).unwrap())
            .collect();
        assert_eq!(per_ip.0.lock().unwrap().len(), 2);
        drop(held);
        assert!(per_ip.acquire(lan).is_some(), "slots were not freed");
        drop((other, local));
        assert!(per_ip.0.lock().unwrap().is_empty());
    }
}
