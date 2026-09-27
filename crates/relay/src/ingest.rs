//! RTMP ingest server: accepts the encoder's publish connection.

use std::collections::{HashMap, VecDeque};
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
/// How long a wrong key waits for its answer once its address has used up its
/// tries ([`MAX_BAD_KEYS`]), and while guesses come from many addresses
/// ([`MAX_BAD_KEYS_OVERALL`]).
const SLOW_REJECT_DELAY: Duration = Duration::from_secs(10);
const ATTACK_REJECT_DELAY: Duration = Duration::from_secs(30);

/// Connections handled at once. Only one can publish; the rest are waiting to be
/// rejected or are stale. When all are taken, the oldest that is not publishing
/// is closed to make room, so an encoder connecting always gets in.
const MAX_CONNECTIONS: usize = 16;

/// Connections from one non-loopback address (for IPv6, one /64) at once, so a
/// single host on the network cannot take every slot. At the limit, the oldest
/// of them that is not publishing is closed to make room.
const MAX_CONNECTIONS_PER_IP: usize = 4;

/// Wrong stream keys one address may send, answered after [`REJECT_DELAY`],
/// before the rest are answered after [`SLOW_REJECT_DELAY`] for
/// [`BAD_KEY_COOLDOWN`]. A correct key is never refused: many clients can share
/// one address (Docker port forwarding, a proxy, carrier-grade NAT), the
/// streamer's encoder among them. What keeps a network-reachable ingest key from
/// being guessed is its strength (see [`crate::ingest_key_weakness`]).
const MAX_BAD_KEYS: u32 = 5;
const BAD_KEY_COOLDOWN: Duration = Duration::from_secs(60);
/// Wrong keys from all addresses together within [`BAD_KEY_WINDOW`] that mean
/// someone is guessing from many: then each address gets one try, and waits
/// [`ATTACK_REJECT_DELAY`] for each answer after it, for [`ATTACK_COOLDOWN`].
const MAX_BAD_KEYS_OVERALL: usize = 20;
const BAD_KEY_WINDOW: Duration = Duration::from_secs(60);
const ATTACK_COOLDOWN: Duration = Duration::from_secs(600);
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
    shutdown: watch::Receiver<bool>,
) {
    listen_as(listener, publish_timeout, events, shutdown, |peer| peer).await;
}

/// [`listen`], taking each connection to come from `peer_of` its address
/// (tests: another address than loopback, whose peers have no limits).
async fn listen_as(
    listener: TcpListener,
    publish_timeout: Duration,
    events: IngestTx,
    mut shutdown: watch::Receiver<bool>,
    peer_of: fn(SocketAddr) -> SocketAddr,
) {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let conns = Conns::default();
    let bad_keys = BadKeys::default();
    loop {
        let accepted = tokio::select! {
            a = listener.accept() => a,
            _ = shutdown.changed() => return,
        };
        match accepted {
            Ok((tcp, peer)) => {
                let peer = peer_of(peer);
                // Loopback peers are local programs: not limited per address.
                let ip = peer.ip().to_canonical();
                let addr = (!ip.is_loopback()).then(|| addr_key(ip));
                let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                let Some(slot) = conns.admit(id, addr) else {
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
    /// See [`addr_key`]; `None` for loopback peers.
    addr: Option<IpAddr>,
    publishing: Arc<AtomicBool>,
    /// Refused, and only waiting to be told: the first to close for room.
    refused: Arc<AtomicBool>,
    close: Arc<Notify>,
}

/// A connection's place in [`Conns`]; leaves it when dropped.
struct ConnSlot {
    conns: Conns,
    id: u64,
    /// Set while the connection is publishing, which keeps it from being closed
    /// to make room.
    publishing: Arc<AtomicBool>,
    /// Set once it was refused (see [`ConnEntry::refused`]).
    refused: Arc<AtomicBool>,
    /// Notified to close the connection.
    close: Arc<Notify>,
}

impl Conns {
    /// Adds a connection from `addr`. When that address, or every slot, is at
    /// its limit, the oldest connection there that is not publishing is closed
    /// to make room (one that was refused first): connections that never
    /// publish only hold a slot until the next one arrives, and an encoder
    /// always gets in, whoever shares its address.
    fn admit(&self, id: u64, addr: Option<IpAddr>) -> Option<ConnSlot> {
        let mut v = self.0.lock().ok()?;
        if let Some(a) = addr
            && v.iter().filter(|e| e.addr == Some(a)).count() >= MAX_CONNECTIONS_PER_IP
        {
            close_oldest_idle(&mut v, |e| e.addr == Some(a))?;
        }
        if v.len() >= MAX_CONNECTIONS {
            close_oldest_idle(&mut v, |_| true)?;
        }
        let publishing = Arc::new(AtomicBool::new(false));
        let refused = Arc::new(AtomicBool::new(false));
        let close = Arc::new(Notify::new());
        v.push(ConnEntry {
            id,
            addr,
            publishing: publishing.clone(),
            refused: refused.clone(),
            close: close.clone(),
        });
        Some(ConnSlot {
            conns: self.clone(),
            id,
            publishing,
            refused,
            close,
        })
    }
}

/// Closes the oldest of the connections `among` that is not publishing,
/// preferring one that was refused. `None` if all are publishing.
fn close_oldest_idle(v: &mut Vec<ConnEntry>, among: impl Fn(&ConnEntry) -> bool) -> Option<()> {
    let idle = |e: &ConnEntry| among(e) && !e.publishing.load(Ordering::Relaxed);
    let i = v
        .iter()
        .position(|e| idle(e) && e.refused.load(Ordering::Relaxed))
        .or_else(|| v.iter().position(idle))?;
    let old = v.remove(i);
    debug!(
        conn = old.id,
        "too many ingest connections; closing the oldest idle one"
    );
    old.close.notify_one();
    Some(())
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        if let Ok(mut v) = self.conns.0.lock() {
            v.retain(|e| e.id != self.id);
        }
    }
}

/// The encoder's `connect` properties to pass on to the destination: all but
/// those we set ourselves. Enhanced RTMP's `fourCcList`, for one, says which
/// codecs may follow.
fn forwarded_props(props: Vec<(String, Amf0Value)>) -> Vec<(String, Amf0Value)> {
    props
        .into_iter()
        .filter(|(k, _)| !OWN_CONNECT_PROPS.contains(&k.as_str()))
        .collect()
}

/// Wrong stream keys per address (see [`addr_key`]), and from all addresses
/// together. Loopback peers are not counted: they are local programs.
#[derive(Clone, Default)]
struct BadKeys(Arc<Mutex<BadKeyLog>>);

#[derive(Default)]
struct BadKeyLog {
    /// Per address: how many in a row, and when the last came.
    per_addr: HashMap<IpAddr, (u32, Instant)>,
    /// When the recent ones came, from any address (within [`BAD_KEY_WINDOW`]).
    recent: VecDeque<Instant>,
}

impl BadKeyLog {
    /// Guessing from many addresses at once: each gets fewer tries, for longer.
    fn under_attack(&mut self, now: Instant) -> bool {
        while self
            .recent
            .front()
            .is_some_and(|t| now.duration_since(*t) >= BAD_KEY_WINDOW)
        {
            self.recent.pop_front();
        }
        self.recent.len() >= MAX_BAD_KEYS_OVERALL
    }

    /// Tries per address, and how long one that used them up waits.
    fn limits(&mut self, now: Instant) -> (u32, Duration) {
        if self.under_attack(now) {
            (1, ATTACK_COOLDOWN)
        } else {
            (MAX_BAD_KEYS, BAD_KEY_COOLDOWN)
        }
    }
}

impl BadKeys {
    /// Records a wrong key from `ip`, and says how long to wait before refusing
    /// it: [`REJECT_DELAY`], or once its address has used up its tries, longer
    /// (without counting it again). Loopback peers' are not recorded.
    fn record(&self, ip: IpAddr) -> Duration {
        let ip = ip.to_canonical();
        if ip.is_loopback() {
            return REJECT_DELAY;
        }
        let addr = addr_key(ip);
        let Ok(mut log) = self.0.lock() else {
            return REJECT_DELAY;
        };
        let now = Instant::now();
        let (tries, cooldown) = log.limits(now);
        if log
            .per_addr
            .get(&addr)
            .is_some_and(|(n, last)| *n >= tries && now.duration_since(*last) < cooldown)
        {
            return if cooldown == ATTACK_COOLDOWN {
                ATTACK_REJECT_DELAY
            } else {
                SLOW_REJECT_DELAY
            };
        }
        let before = log.under_attack(now);
        // The latest ones, enough to tell.
        if log.recent.len() >= MAX_BAD_KEYS_OVERALL {
            log.recent.pop_front();
        }
        log.recent.push_back(now);
        if !before && log.under_attack(now) {
            warn!(
                "wrong stream keys from many addresses: someone may be guessing the ingest \
                 key; each address now gets one quick try every {} min",
                ATTACK_COOLDOWN.as_secs() / 60
            );
        }
        // Remembered as long as they may count.
        log.per_addr
            .retain(|_, (_, last)| now.duration_since(*last) < ATTACK_COOLDOWN);
        if log.per_addr.len() >= MAX_TRACKED_ADDRESSES
            && !log.per_addr.contains_key(&addr)
            && let Some(oldest) = log
                .per_addr
                .iter()
                .min_by_key(|(_, (_, t))| *t)
                .map(|(a, _)| *a)
        {
            log.per_addr.remove(&oldest);
        }
        let (tries, cooldown) = log.limits(now);
        let entry = log.per_addr.entry(addr).or_insert((0, now));
        // An address that kept quiet for the cooldown starts afresh.
        if now.duration_since(entry.1) >= BAD_KEY_COOLDOWN {
            entry.0 = 0;
        }
        entry.0 += 1;
        entry.1 = now;
        if entry.0 == tries {
            warn!(
                %addr,
                "too many wrong stream keys from this address; the next are answered slowly \
                 for {} s",
                cooldown.as_secs()
            );
        }
        REJECT_DELAY
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
                    connect_props = forwarded_props(props);
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
                            slot.refused.store(true, Ordering::Relaxed);
                            slot.publishing.store(false, Ordering::Relaxed);
                            warn!(%peer, "rejected publish: {}", rejection.reason);
                            let wait = if rejection.bad_key {
                                bad_keys.record(peer.ip())
                            } else {
                                REJECT_DELAY
                            };
                            // Answer slowly: an address gets only a few quick tries.
                            tokio::time::sleep(wait).await;
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

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn ipv6_hosts_count_per_64() {
        let a = ip("2001:db8:1:2:aaaa::1");
        let b = ip("2001:db8:1:2:bbbb::9");
        assert_eq!(addr_key(a), addr_key(b));
        assert_ne!(addr_key(a), addr_key(ip("2001:db8:1:3::1")));
        assert_eq!(addr_key(ip("::ffff:192.0.2.7")), ip("192.0.2.7"));
    }

    #[test]
    fn an_address_that_used_its_tries_is_answered_slowly_not_refused() {
        let bad = BadKeys::default();
        let a = ip("192.0.2.7");
        for _ in 0..MAX_BAD_KEYS {
            assert_eq!(bad.record(a), REJECT_DELAY);
        }
        // The next wait longer, and do not count again: the slow spell ends a
        // cooldown after the last quick try, however many come meanwhile.
        let counted = |bad: &BadKeys| bad.0.lock().unwrap().per_addr[&addr_key(a)];
        let before = counted(&bad);
        for _ in 0..10 {
            assert_eq!(bad.record(a), SLOW_REJECT_DELAY);
        }
        assert_eq!(counted(&bad), before);
        assert_eq!(bad.record(ip("192.0.2.8")), REJECT_DELAY, "other addresses");
        // Addresses that stay quiet start afresh.
        bad.0
            .lock()
            .unwrap()
            .per_addr
            .get_mut(&addr_key(a))
            .unwrap()
            .1 = Instant::now() - BAD_KEY_COOLDOWN;
        assert_eq!(bad.record(a), REJECT_DELAY);
    }

    #[test]
    fn wrong_keys_count_per_address_but_not_from_this_computer() {
        let bad = BadKeys::default();
        for local in ["127.0.0.1", "::1", "::ffff:127.0.0.1"] {
            for _ in 0..MAX_BAD_KEYS * 2 {
                assert_eq!(bad.record(ip(local)), REJECT_DELAY, "{local}");
            }
        }
        assert!(bad.0.lock().unwrap().per_addr.is_empty());
        // Addresses in one IPv6 /64 share their tries.
        for i in 1..=MAX_BAD_KEYS {
            bad.record(ip(&format!("2001:db8:1:2::{i}")));
        }
        assert_eq!(bad.record(ip("2001:db8:1:2::ffff")), SLOW_REJECT_DELAY);
        assert_eq!(bad.record(ip("2001:db8:1:3::1")), REJECT_DELAY);
    }

    #[test]
    fn the_addresses_remembered_are_bounded() {
        let bad = BadKeys::default();
        for i in 0..MAX_TRACKED_ADDRESSES as u32 + 10 {
            bad.record(IpAddr::from(std::net::Ipv4Addr::from(0x0a00_0000 + i)));
        }
        assert_eq!(bad.0.lock().unwrap().per_addr.len(), MAX_TRACKED_ADDRESSES);
    }

    #[test]
    fn guessing_from_many_addresses_leaves_each_one_quick_try() {
        let bad = BadKeys::default();
        let addr = |i: u32| IpAddr::from(std::net::Ipv4Addr::from(0xc633_6400 + i));
        // Each stays within its own tries...
        for i in 0..MAX_BAD_KEYS_OVERALL as u32 {
            assert_eq!(bad.record(addr(i)), REJECT_DELAY);
        }
        // ...but together they are too many: after one, the rest wait longest.
        assert_eq!(bad.record(addr(0)), ATTACK_REJECT_DELAY);
        assert_eq!(bad.record(addr(2000)), REJECT_DELAY, "a first try");
        assert_eq!(bad.record(addr(2000)), ATTACK_REJECT_DELAY);
        // Once the guessing stops, the usual limits are back.
        bad.0.lock().unwrap().recent.clear();
        assert_eq!(bad.record(addr(0)), REJECT_DELAY);
    }

    #[test]
    fn the_destination_gets_the_encoders_own_connect_properties() {
        let props = [
            "app",
            "tcUrl",
            "flashVer",
            "swfUrl",
            "type",
            "fourCcList",
            "videoCodecs",
        ]
        .map(|k| (k.to_string(), Amf0Value::Null));
        let kept: Vec<String> = forwarded_props(props.to_vec())
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(kept, ["fourCcList", "videoCodecs"]);
    }

    fn ids(conns: &Conns) -> Vec<u64> {
        conns.0.lock().unwrap().iter().map(|e| e.id).collect()
    }

    #[test]
    fn a_closed_connection_leaves_only_its_own_slot() {
        let conns = Conns::default();
        let mut slots: Vec<ConnSlot> = (0..3).map(|id| conns.admit(id, None).unwrap()).collect();
        drop(slots.remove(1));
        assert_eq!(ids(&conns), [0, 2]);
    }

    #[test]
    fn full_slots_close_the_oldest_idle_connection() {
        let conns = Conns::default();
        let slots: Vec<ConnSlot> = (0..MAX_CONNECTIONS as u64)
            .map(|id| conns.admit(id, None).unwrap())
            .collect();
        // The oldest is publishing, so the next oldest makes room.
        slots[0].publishing.store(true, Ordering::Relaxed);
        let newcomer = conns.admit(100, None).expect("no room made");
        let ids = ids(&conns);
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
    fn one_address_at_its_limit_makes_room_among_its_own_connections() {
        let conns = Conns::default();
        let lan = Some(addr_key(ip("2001:db8:1:2::1")));
        let slots: Vec<ConnSlot> = (0..MAX_CONNECTIONS_PER_IP as u64)
            .map(|id| conns.admit(id, lan).unwrap())
            .collect();
        let other = conns.admit(50, Some(ip("192.0.2.8"))).unwrap();
        // The oldest publishes; the third was refused and only waits to be told.
        slots[0].publishing.store(true, Ordering::Relaxed);
        slots[2].refused.store(true, Ordering::Relaxed);
        let _a = conns.admit(100, lan).expect("no room made");
        assert_eq!(
            ids(&conns),
            [0, 1, 3, 50, 100],
            "the refused one goes first"
        );
        let _b = conns.admit(101, lan).expect("no room made");
        assert_eq!(ids(&conns), [0, 3, 50, 100, 101], "then the oldest idle");
        // Loopback peers are not limited per address.
        let local: Vec<ConnSlot> = (0..MAX_CONNECTIONS_PER_IP as u64 * 2)
            .map(|i| conns.admit(200 + i, None).unwrap())
            .collect();
        assert_eq!(ids(&conns).len(), 5 + local.len());
        // All of one address publishing (answers on their way): no room.
        drop(local);
        let busy: Vec<ConnSlot> = (0..MAX_CONNECTIONS_PER_IP as u64)
            .map(|i| conns.admit(300 + i, Some(ip("192.0.2.9"))).unwrap())
            .collect();
        busy.iter()
            .for_each(|s| s.publishing.store(true, Ordering::Relaxed));
        assert!(conns.admit(400, Some(ip("192.0.2.9"))).is_none());
        drop((slots, other, busy));
    }

    /// An encoder connecting to `addr` and asking to publish with `key`, for
    /// as long as it is kept; the answer says whether it may.
    async fn publish(addr: SocketAddr, key: &str) -> (TcpStream, bool) {
        use bytes::BytesMut;
        use streamdelay_rtmp::handshake::{ClientHandshake, Progress};
        use streamdelay_rtmp::session::{ClientConfig, ClientEvent, ClientSession};

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
            let events = session.feed(&data).unwrap();
            tcp.write_all(&session.take_output()).await.unwrap();
            if events.contains(&ClientEvent::Publishing) {
                return (tcp, true);
            }
            if events
                .iter()
                .any(|e| matches!(e, ClientEvent::Error { .. }))
            {
                return (tcp, false);
            }
            let n = tcp.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                return (tcp, false);
            }
            data = buf[..n].to_vec();
        }
    }

    #[tokio::test]
    async fn the_encoder_gets_in_whatever_its_address_sends() {
        // Everything comes from one address, as behind Docker's port
        // forwarding or a shared NAT: a guesser, then the streamer's encoder.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let ingest = IngestTx::new(
            events_tx,
            1 << 20,
            streamdelay_rtmp::ArenaPool::new(1 << 24),
        );
        let (_stop, shutdown) = watch::channel(false);
        tokio::spawn(listen_as(
            listener,
            Duration::from_secs(5),
            ingest,
            shutdown,
            |p| SocketAddr::new("192.0.2.7".parse().unwrap(), p.port()),
        ));
        // Stands in for the core: one key is right.
        tokio::spawn(async move {
            while let Some(ev) = events_rx.recv().await {
                if let Event::IngestPublish { key, reply, .. } = ev {
                    let _ = reply.send(if key == "right" {
                        Ok(())
                    } else {
                        Err(crate::core::Rejection {
                            reason: "wrong stream key".into(),
                            bad_key: true,
                        })
                    });
                }
            }
        });
        // The guesser uses up the address's tries and keeps its slots busy.
        let mut guesses = Vec::new();
        for i in 0..MAX_BAD_KEYS as usize + MAX_CONNECTIONS_PER_IP {
            let key = format!("guess-{i}");
            guesses.push(tokio::spawn(async move { publish(addr, &key).await }));
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let started = Instant::now();
        let (_encoder, accepted) =
            tokio::time::timeout(Duration::from_secs(2), publish(addr, "right"))
                .await
                .expect("the encoder was held up");
        assert!(accepted, "the encoder was refused");
        assert!(started.elapsed() < Duration::from_secs(2));
        for g in guesses {
            g.abort();
        }
    }
}
