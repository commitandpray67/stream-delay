//! RTMP ingest server: accepts the encoder's publish connection.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use streamdelay_engine::Kind;
use streamdelay_rtmp::amf0::Amf0Value;
use streamdelay_rtmp::session::{MediaKind, ServerConfig, ServerEvent, ServerSession};
use streamdelay_rtmp::ts::TsUnwrapper;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, oneshot, watch};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::core::{Event, IngestTx, untrusted};
use crate::io;

/// If the encoder sends nothing for this long, the connection is considered dead.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a refused publish (for example a wrong ingest key) waits for its answer.
const REJECT_DELAY: Duration = Duration::from_secs(1);

/// Connections handled at once. Only one can publish; the rest are waiting to be
/// rejected or are stale. When all are taken, the oldest that is not publishing
/// is closed to make room, so an encoder connecting always gets in.
const MAX_CONNECTIONS: usize = 16;

/// Connections from one non-loopback address (for IPv6, one /64) at once, so a
/// single host on the network cannot take every slot.
const MAX_CONNECTIONS_PER_IP: usize = 4;

/// Wrong stream keys one address may send before it has to wait
/// [`BAD_KEY_COOLDOWN`]. Together with the connection limits this keeps a
/// network-reachable ingest key from being guessed.
const MAX_BAD_KEYS: u32 = 5;
const BAD_KEY_COOLDOWN: Duration = Duration::from_secs(60);
/// Addresses whose wrong keys are remembered at once.
const MAX_TRACKED_ADDRESSES: usize = 4096;

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
    events: IngestTx,
    mut shutdown: watch::Receiver<bool>,
) {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let conns = Conns::default();
    let per_ip = PerIp::default();
    let bad_keys = BadKeys::default();
    loop {
        let accepted = tokio::select! {
            a = listener.accept() => a,
            _ = shutdown.changed() => return,
        };
        match accepted {
            Ok((tcp, peer)) => {
                let ip = peer.ip().to_canonical();
                if !ip.is_loopback() && bad_keys.blocked(addr_key(ip)) {
                    debug!(%peer, "address is cooling down after wrong stream keys; refusing");
                    continue;
                }
                let Some(ip_slot) = per_ip.acquire(ip) else {
                    debug!(%peer, "too many ingest connections from this address; refusing");
                    continue;
                };
                let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                let Some(slot) = conns.admit(id) else {
                    warn!(%peer, "too many ingest connections; refusing");
                    continue;
                };
                let events = events.clone();
                let bad_keys = bad_keys.clone();
                let mut shutdown = shutdown.clone();
                tokio::spawn(async move {
                    info!(%peer, "encoder connected");
                    let close = slot.close.clone();
                    let result = tokio::select! {
                        r = handle(tcp, peer, id, publish_timeout, events.clone(), &slot, &bad_keys) => r,
                        _ = shutdown.changed() => Ok(()),
                        // Closed to make room for a new connection, or replaced by
                        // the encoder's newer connection.
                        _ = close.notified() => {
                            debug!(%peer, "connection closed by stream-delay");
                            Ok(())
                        }
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
                    let _ = events.send(Event::IngestClosed {
                        conn: id,
                        error,
                        unpublished: false,
                    });
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

/// The address connections are counted by. An IPv6 host usually has a whole /64
/// to pick addresses from, so the /64 counts as one.
fn addr_key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => {
            let s = v6.segments();
            IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
        }
        v4 => v4,
    }
}

/// Open connections, oldest first.
#[derive(Clone, Default)]
struct Conns(Arc<Mutex<Vec<ConnEntry>>>);

struct ConnEntry {
    id: u64,
    publishing: Arc<AtomicBool>,
    close: Arc<Notify>,
}

/// A connection's place in [`Conns`]; leaves it when dropped.
struct ConnSlot {
    conns: Conns,
    id: u64,
    /// Set while the connection is publishing, which keeps it from being closed
    /// to make room.
    publishing: Arc<AtomicBool>,
    /// Notified to close the connection.
    close: Arc<Notify>,
}

impl Conns {
    /// Adds a connection. When every slot is taken, the oldest connection that is
    /// not publishing is closed to make room: connections that never publish only
    /// hold a slot until the next one arrives, and an encoder always gets in.
    fn admit(&self, id: u64) -> Option<ConnSlot> {
        let mut v = self.0.lock().ok()?;
        if v.len() >= MAX_CONNECTIONS {
            let i = v
                .iter()
                .position(|e| !e.publishing.load(Ordering::Relaxed))?;
            let old = v.remove(i);
            debug!(
                conn = old.id,
                "too many ingest connections; closing the oldest idle one"
            );
            old.close.notify_one();
        }
        let publishing = Arc::new(AtomicBool::new(false));
        let close = Arc::new(Notify::new());
        v.push(ConnEntry {
            id,
            publishing: publishing.clone(),
            close: close.clone(),
        });
        Some(ConnSlot {
            conns: self.clone(),
            id,
            publishing,
            close,
        })
    }
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        if let Ok(mut v) = self.conns.0.lock() {
            v.retain(|e| e.id != self.id);
        }
    }
}

/// Wrong stream keys per address.
#[derive(Clone, Default)]
struct BadKeys(Arc<Mutex<HashMap<IpAddr, (u32, Instant)>>>);

impl BadKeys {
    /// True while `addr` has used up its tries.
    fn blocked(&self, addr: IpAddr) -> bool {
        self.0.lock().is_ok_and(|m| {
            m.get(&addr)
                .is_some_and(|(n, last)| *n >= MAX_BAD_KEYS && last.elapsed() < BAD_KEY_COOLDOWN)
        })
    }

    /// Records a wrong key from `addr`. Returns true when this one used up its
    /// tries.
    fn record(&self, addr: IpAddr) -> bool {
        let Ok(mut m) = self.0.lock() else {
            return false;
        };
        let now = Instant::now();
        // An address that has kept quiet for the cooldown starts afresh.
        m.retain(|_, (_, last)| now.duration_since(*last) < BAD_KEY_COOLDOWN);
        if m.len() >= MAX_TRACKED_ADDRESSES
            && !m.contains_key(&addr)
            && let Some(oldest) = m.iter().min_by_key(|(_, (_, t))| *t).map(|(a, _)| *a)
        {
            m.remove(&oldest);
        }
        let entry = m.entry(addr).or_insert((0, now));
        entry.0 += 1;
        entry.1 = now;
        entry.0 == MAX_BAD_KEYS
    }
}

/// Counts connections per remote address (see [`addr_key`]). Loopback peers are
/// not limited: they are local programs, and the global limit still applies.
#[derive(Clone, Default)]
struct PerIp(Arc<Mutex<HashMap<IpAddr, usize>>>);

struct IpSlot {
    map: PerIp,
    /// `None` for loopback peers, which are not counted.
    ip: Option<IpAddr>,
}

impl PerIp {
    fn acquire(&self, ip: IpAddr) -> Option<IpSlot> {
        let ip = (!ip.is_loopback()).then(|| addr_key(ip));
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
    events: IngestTx,
    slot: &ConnSlot,
    bad_keys: &BadKeys,
) -> std::io::Result<()> {
    io::tune(&tcp);
    // Set while not publishing: the time by which the connection must publish.
    let mut deadline = Some(Instant::now() + publish_timeout);
    let not_publishing = || timed_out("the encoder did not start publishing in time");
    let rest = tokio::time::timeout(publish_timeout, io::server_handshake(&mut tcp))
        .await
        .map_err(|_| not_publishing())??;
    let mut session = ServerSession::new(ServerConfig::default());
    session.set_arena_pool(events.arena.clone());
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
                    info!(
                        %peer,
                        app = %untrusted(&app),
                        encoder = %untrusted(encoder),
                        "encoder sent connect"
                    );
                    connect_props = props
                        .into_iter()
                        .filter(|(k, _)| !OWN_CONNECT_PROPS.contains(&k.as_str()))
                        .collect();
                }
                ServerEvent::PublishRequest { app, stream_key } => {
                    info!(%peer, app = %untrusted(&app), "encoder asked to publish");
                    let (tx, rx) = oneshot::channel();
                    // Not to be closed to make room while the answer is on its way.
                    slot.publishing.store(true, Ordering::Relaxed);
                    let _ = events.send(Event::IngestPublish {
                        conn,
                        peer,
                        app,
                        key: stream_key,
                        connect_props: connect_props.clone(),
                        close: slot.close.clone(),
                        reply: tx,
                    });
                    match rx.await {
                        Ok(Ok(())) => {
                            info!(%peer, "encoder started publishing");
                            session.accept_publish();
                            publishing = true;
                            deadline = None;
                        }
                        Ok(Err(rejection)) => {
                            slot.publishing.store(false, Ordering::Relaxed);
                            warn!(%peer, "rejected publish: {}", rejection.reason);
                            let ip = peer.ip().to_canonical();
                            if rejection.bad_key
                                && !ip.is_loopback()
                                && bad_keys.record(addr_key(ip))
                            {
                                warn!(
                                    %peer,
                                    "too many wrong stream keys from this address; refusing it for {} s",
                                    BAD_KEY_COOLDOWN.as_secs()
                                );
                            }
                            // Answer slowly: with the per-address limits, one address
                            // gets only a few tries.
                            tokio::time::sleep(REJECT_DELAY).await;
                            session.reject_publish("NetStream.Publish.BadName", &rejection.reason);
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
                    let Some(permit) = events.reserve(payload.len()).await else {
                        return Ok(());
                    };
                    let _ = events.send(Event::IngestMedia {
                        conn,
                        kind: k,
                        ts,
                        payload,
                        _permit: permit,
                    });
                }
                ServerEvent::Data { timestamp, payload } => {
                    let ts = unwrap[2].unwrap(timestamp);
                    let Some(permit) = events.reserve(payload.len()).await else {
                        return Ok(());
                    };
                    let _ = events.send(Event::IngestMedia {
                        conn,
                        kind: Kind::Data,
                        ts,
                        payload,
                        _permit: permit,
                    });
                }
                ServerEvent::Metadata { payload, .. } => {
                    let Some(permit) = events.reserve(payload.len()).await else {
                        return Ok(());
                    };
                    let _ = events.send(Event::IngestMetadata {
                        conn,
                        payload,
                        _permit: permit,
                    });
                }
                ServerEvent::Unpublish => {
                    if publishing {
                        info!(%peer, "encoder stopped publishing");
                        publishing = false;
                        slot.publishing.store(false, Ordering::Relaxed);
                        deadline = Some(Instant::now() + publish_timeout);
                        let _ = events.send(Event::IngestClosed {
                            conn,
                            error: None,
                            unpublished: true,
                        });
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
    fn ipv6_hosts_count_per_64() {
        let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:bbbb::9".parse().unwrap();
        let other: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(addr_key(a), addr_key(b));
        assert_ne!(addr_key(a), addr_key(other));
        let mapped: IpAddr = "::ffff:192.0.2.7".parse().unwrap();
        assert_eq!(addr_key(mapped), "192.0.2.7".parse::<IpAddr>().unwrap());
        let per_ip = PerIp::default();
        let held: Vec<IpSlot> = (0..MAX_CONNECTIONS_PER_IP)
            .map(|i| {
                let ip: IpAddr = format!("2001:db8:1:2::{}", i + 1).parse().unwrap();
                per_ip.acquire(ip).unwrap()
            })
            .collect();
        assert!(
            per_ip.acquire(b).is_none(),
            "a new address in the same /64 got in"
        );
        drop(held);
    }

    #[test]
    fn wrong_keys_put_an_address_on_hold() {
        let bad = BadKeys::default();
        let a: IpAddr = "192.0.2.7".parse().unwrap();
        let b: IpAddr = "192.0.2.8".parse().unwrap();
        for i in 1..MAX_BAD_KEYS {
            assert!(!bad.record(a), "on hold after {i}");
            assert!(!bad.blocked(a));
        }
        assert!(bad.record(a), "the last try should put it on hold");
        assert!(bad.blocked(a));
        assert!(!bad.blocked(b), "other addresses are not affected");
        // Addresses that stay quiet are forgotten.
        if let Ok(mut m) = bad.0.lock() {
            m.get_mut(&a).unwrap().1 = Instant::now() - BAD_KEY_COOLDOWN;
        }
        assert!(!bad.blocked(a));
    }

    #[test]
    fn full_slots_close_the_oldest_idle_connection() {
        let conns = Conns::default();
        let slots: Vec<ConnSlot> = (0..MAX_CONNECTIONS as u64)
            .map(|id| conns.admit(id).unwrap())
            .collect();
        // The oldest is publishing, so the next oldest makes room.
        slots[0].publishing.store(true, Ordering::Relaxed);
        let newcomer = conns.admit(100).expect("no room made");
        let ids: Vec<u64> = conns.0.lock().unwrap().iter().map(|e| e.id).collect();
        assert_eq!(ids.len(), MAX_CONNECTIONS);
        assert!(ids.contains(&0), "the publishing connection was closed");
        assert!(
            !ids.contains(&1),
            "the oldest idle connection is still open"
        );
        assert!(ids.contains(&100));
        drop((slots, newcomer));
        assert!(conns.0.lock().unwrap().is_empty());
    }

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
