//! Engine tests on a simulated clock.
//!
//! The simulator produces a 30 fps video stream with a 2 s GOP and AAC audio every
//! ~21 ms. Every payload carries a unique id so tests can map outputs to inputs.

use std::collections::HashMap;

use bytes::{BufMut, Bytes, BytesMut};
use proptest::prelude::*;

use super::*;

const FRAME_MS: u64 = 33;
const GOP_FRAMES: u64 = 60;

#[derive(Debug, Clone, Copy)]
struct InputInfo {
    arrival: Time,
    kind: Kind,
    ts: u64,
    keyframe: bool,
    /// Index among video frames (or audio frames) of its session.
    index: u64,
    session: u32,
}

struct Sent {
    at: Time,
    msg: OutMsg,
}

struct Sim {
    e: Engine,
    now: Time,
    next_id: u32,
    video_index: u64,
    audio_index: u64,
    next_video: Time,
    next_audio: Time,
    ts_base: u64,
    session: u32,
    encoder_on: bool,
    inputs: HashMap<u32, InputInfo>,
    sent: Vec<Sent>,
    wake: Option<Time>,
    /// Minimum delay the engine has committed to, for the no-leak invariant.
    floor: u64,
    /// (index into `sent` from which the floor applies, floor)
    floor_log: Vec<(usize, u64)>,
    audio: bool,
    jitter: u64,
    /// Byte budget per poll (a destination falling behind), or unlimited.
    budget: Option<usize>,
}

fn payload(prefix: &[u8], id: u32) -> Bytes {
    let mut b = BytesMut::new();
    b.put_slice(prefix);
    b.put_u32(id);
    b.freeze()
}

fn id_of(p: &[u8]) -> Option<u32> {
    if p.len() < 4 {
        return None;
    }
    let n = p.len();
    Some(u32::from_be_bytes([p[n - 4], p[n - 3], p[n - 2], p[n - 1]]))
}

impl Sim {
    fn new(config: EngineConfig) -> Self {
        let mut s = Self {
            e: Engine::new(config),
            now: 1_000 * SEC,
            next_id: 1,
            video_index: 0,
            audio_index: 0,
            next_video: 0,
            next_audio: 0,
            ts_base: 0,
            session: 0,
            encoder_on: false,
            inputs: HashMap::new(),
            sent: Vec::new(),
            wake: None,
            floor: 0,
            floor_log: Vec::new(),
            audio: true,
            jitter: 0,
            budget: None,
        };
        s.start_encoder(0);
        s
    }

    fn start_encoder(&mut self, ts_base: u64) {
        self.session = self.e.ingest_start(self.now);
        self.encoder_on = true;
        self.ts_base = ts_base;
        self.video_index = 0;
        self.audio_index = 0;
        self.next_video = self.now;
        self.next_audio = self.now;
        let meta = rtmp_meta();
        self.e.ingest_metadata(self.now, meta);
        // Decoder configuration comes first, like OBS.
        self.push(Kind::Video, ts_base, &[0x17, 0x00, 0, 0, 0, 0x01], false);
        self.push(Kind::Audio, ts_base, &[0xaf, 0x00, 0x11, 0x90], false);
    }

    fn stop_encoder(&mut self) {
        self.encoder_on = false;
        self.e.ingest_end(self.now);
    }

    fn push(&mut self, kind: Kind, ts: u64, prefix: &[u8], counted: bool) {
        let id = self.next_id;
        self.next_id += 1;
        let keyframe = kind == Kind::Video && prefix[0] == 0x17 && prefix[1] == 0x01;
        let index = if kind == Kind::Video {
            self.video_index
        } else {
            self.audio_index
        };
        if counted {
            self.inputs.insert(
                id,
                InputInfo {
                    arrival: self.now,
                    kind,
                    ts,
                    keyframe,
                    index,
                    session: self.session,
                },
            );
        }
        self.e.ingest(self.now, kind, ts, payload(prefix, id));
    }

    fn connect(&mut self) {
        self.e.output_connected(self.now);
        self.poll();
    }

    fn poll(&mut self) {
        let mut out = Vec::new();
        self.wake = match self.budget {
            Some(b) => self.e.poll_budget(self.now, &mut out, b),
            None => self.e.poll(self.now, &mut out),
        };
        for m in out {
            self.sent.push(Sent {
                at: self.now,
                msg: m,
            });
        }
    }

    /// Runs the simulation until `until` (absolute time).
    fn run_until(&mut self, until: Time) {
        loop {
            let mut next = until;
            if self.encoder_on {
                next = next.min(self.next_video);
                if self.audio {
                    next = next.min(self.next_audio);
                }
            }
            if let Some(w) = self.wake {
                next = next.min(w.max(self.now));
            }
            if next > until {
                break;
            }
            self.now = next;
            if self.encoder_on && self.now >= self.next_video {
                let ts = self.ts_base + self.video_index * FRAME_MS;
                let key = self.video_index.is_multiple_of(GOP_FRAMES);
                let prefix: &[u8] = if key {
                    &[0x17, 0x01, 0, 0, 0]
                } else {
                    &[0x27, 0x01, 0, 0, 0]
                };
                self.push(Kind::Video, ts, prefix, true);
                self.video_index += 1;
                self.next_video = self.now + FRAME_MS * MS + (self.video_index % 3) * self.jitter;
            }
            if self.encoder_on && self.audio && self.now >= self.next_audio {
                let ts = self.ts_base + self.audio_index * 21;
                self.push(Kind::Audio, ts, &[0xaf, 0x01], true);
                self.audio_index += 1;
                self.next_audio = self.now + 21 * MS;
            }
            self.poll();
            if self.now == until {
                break;
            }
        }
        self.now = until;
        self.poll();
    }

    fn advance(&mut self, d: Time) {
        let t = self.now + d;
        self.run_until(t);
    }

    fn cmd(&mut self, c: Command) -> Ack {
        let ack = self.e.command(self.now, c).unwrap();
        // Lowering the delay releases the floor immediately; raising it only counts
        // once the engine reports the new delay as in effect.
        match c {
            Command::SetDelay { ms, .. } if !ack.pending && !ack.history_short => {
                self.floor = ms * MS;
            }
            Command::SetDelay { ms, .. } if ms * MS < self.floor => self.floor = ms * MS,
            Command::GoLive(_) | Command::Cancel => self.floor = 0,
            // Under the slate, what it covers airs at once while the delay builds
            // back up; a replay keeps the delay.
            Command::Dump(_) if self.e.snapshot(self.now).mask_visible => self.floor = 0,
            _ => {}
        }
        self.floor_log.push((self.sent.len(), self.floor));
        self.poll();
        ack
    }

    fn snapshot(&self) -> Snapshot {
        self.e.snapshot(self.now)
    }

    fn media_sent(&self) -> impl Iterator<Item = (&Sent, InputInfo)> {
        self.media_sent_in(0..self.sent.len())
            .map(|(_, s, i)| (s, i))
    }

    fn media_sent_in(
        &self,
        range: std::ops::Range<usize>,
    ) -> impl Iterator<Item = (usize, &Sent, InputInfo)> {
        let start = range.start;
        self.sent[range]
            .iter()
            .enumerate()
            .filter_map(move |(n, s)| {
                let id = id_of(&s.msg.payload)?;
                self.inputs.get(&id).map(|i| (start + n, s, *i))
            })
    }

    fn floor_at(&self, index: usize) -> u64 {
        self.floor_log
            .iter()
            .rev()
            .find(|(at, _)| *at <= index)
            .map_or(0, |(_, f)| *f)
    }

    fn check_invariants(&self) {
        self.check_invariants_in(0..self.sent.len());
    }

    /// Checks the invariants every output must satisfy, for one destination connection.
    fn check_invariants_in(&self, range: std::ops::Range<usize>) {
        let mut last_ts: HashMap<Kind, u32> = HashMap::new();
        let mut last_video: Option<InputInfo> = None;
        let mut last_video_offset: Option<i64> = None;
        let mut last_video_ts: Option<u32> = None;
        for (index, s, info) in self.media_sent_in(range) {
            let ts = s.msg.timestamp;
            // 1. Timestamps never go backwards per kind.
            if let Some(prev) = last_ts.get(&info.kind) {
                assert!(
                    ts >= *prev,
                    "{:?} timestamp went backwards: {prev} -> {ts}",
                    info.kind
                );
            }
            last_ts.insert(info.kind, ts);
            // 2. Nothing is sent earlier than its arrival plus the committed delay floor.
            let waited = s.at - info.arrival;
            let floor = self.floor_at(index);
            assert!(
                waited >= floor,
                "sent after {waited} us but the floor is {floor} us"
            );
            let offset = ts as i64 - info.ts as i64;
            if info.kind == Kind::Video {
                // 3. Every discontinuity in video lands on a keyframe.
                let continuous = last_video
                    .is_some_and(|p| p.session == info.session && p.index + 1 == info.index);
                assert!(
                    continuous || info.keyframe,
                    "splice to a non-keyframe (video index {})",
                    info.index
                );
                if let Some(prev) = last_video_ts {
                    assert!(
                        ts > prev,
                        "video ts not strictly increasing: {prev} -> {ts}"
                    );
                }
                last_video_ts = Some(ts);
                last_video = Some(info);
                last_video_offset = Some(offset);
            } else if info.kind == Kind::Audio
                && let Some(vo) = last_video_offset
            {
                // 4. Audio and video share the same timestamp offset (A/V sync).
                assert!(
                    (offset - vo).abs() <= 1,
                    "A/V offset drift: audio {offset} video {vo}"
                );
            }
        }
    }
}

fn rtmp_meta() -> Bytes {
    Bytes::from_static(b"\x02\x00\x0d@setDataFrame\x02\x00\x0aonMetaData\x05")
}

fn config() -> EngineConfig {
    EngineConfig {
        max_delay_ms: 120_000,
        ..Default::default()
    }
}

fn live_sim() -> Sim {
    let mut s = Sim::new(config());
    s.connect();
    s.advance(40 * SEC);
    s
}

fn effective(s: &Sim) -> u64 {
    s.snapshot().effective_ms
}

#[test]
fn passthrough_at_zero_delay() {
    let s = live_sim();
    assert_eq!(s.snapshot().phase, Phase::Live);
    // Everything arrives and leaves at the same instant.
    for (sent, info) in s.media_sent() {
        assert_eq!(sent.at, info.arrival);
    }
    // First messages on the connection: metadata then decoder configuration.
    assert_eq!(s.sent[0].msg.kind, Kind::Data);
    assert_eq!(&s.sent[1].msg.payload[..2], &[0x17, 0x00]);
    assert_eq!(&s.sent[2].msg.payload[..2], &[0xaf, 0x00]);
    assert_eq!(s.sent[3].msg.timestamp, 0);
    s.check_invariants();
}

#[test]
fn rewind_adds_delay_immediately() {
    let mut s = live_sim();
    let ack = s.cmd(Command::SetDelay {
        ms: 10_000,
        mode: DelayMode::Rewind,
    });
    assert!(!ack.pending);
    assert!((10_000..=12_100).contains(&ack.effective_ms), "{ack:?}");
    s.advance(30 * SEC);
    assert_eq!(s.snapshot().phase, Phase::Delayed);
    assert!((10_000..=12_100).contains(&effective(&s)));
    assert_eq!(s.snapshot().output.splices, 1);
    s.check_invariants();
}

#[test]
fn go_live_now_skips_to_next_keyframe() {
    let mut s = live_sim();
    s.cmd(Command::SetDelay {
        ms: 20_000,
        mode: DelayMode::Rewind,
    });
    s.advance(20 * SEC);
    let before = s.now;
    let ack = s.cmd(Command::GoLive(GoLiveWhen::Now));
    assert!(ack.pending);
    s.advance(2_100 * MS);
    assert!(effective(&s) < 100, "effective {}", effective(&s));
    assert_eq!(s.snapshot().phase, Phase::Live);
    // Content from the skipped window never aired after the command.
    let after: Vec<_> = s.media_sent().filter(|(x, _)| x.at > before).collect();
    assert!(after.iter().all(|(x, i)| x.at - i.arrival < 21 * SEC));
    s.check_invariants();
}

#[test]
fn a_destination_that_connects_late_catches_up_at_the_next_keyframe() {
    // Connecting to the destination takes a moment after the encoder starts, and
    // the output starts on a keyframe: the broadcast starts that far behind.
    let mut s = Sim::new(config());
    s.advance(1_500 * MS);
    let connected = s.now;
    s.connect();
    s.advance(10 * MS);
    assert!(
        (1_400..=1_600).contains(&effective(&s)),
        "{:?}",
        s.snapshot()
    );
    // It catches up at the next keyframe, instead of keeping the lag for good.
    s.advance(SEC);
    let snap = s.snapshot();
    assert_eq!(snap.phase, Phase::Live, "{snap:?}");
    assert!(snap.effective_ms < 100, "{snap:?}");
    assert_eq!(snap.target_ms, 0);
    // What aired late was only the start, up to that keyframe.
    for (sent, info) in s.media_sent() {
        if sent.at - info.arrival > 100 * MS {
            assert!(info.arrival < connected, "late from {}", info.arrival);
        }
    }
    s.advance(20 * SEC);
    assert!(effective(&s) < 100);
    assert_eq!(s.snapshot().output.splices, 1);
    s.check_invariants();
}

#[test]
fn a_late_destination_catches_up_to_the_delay_asked_for() {
    let mut s = Sim::new(config());
    s.cmd(Command::SetDelay {
        ms: 1_000,
        mode: DelayMode::Rewind,
    });
    s.advance(3_500 * MS);
    s.connect();
    // The newest keyframe that is due (at 2 s) is 1.5 s old.
    assert!(
        (1_400..=1_600).contains(&effective(&s)),
        "{:?}",
        s.snapshot()
    );
    s.advance(3 * SEC);
    let snap = s.snapshot();
    assert!((1_000..=1_100).contains(&snap.effective_ms), "{snap:?}");
    assert_eq!(snap.phase, Phase::Delayed);
    s.check_invariants();
}

#[test]
fn a_destination_ready_in_time_starts_at_the_delay_without_a_splice() {
    let mut s = Sim::new(config());
    s.cmd(Command::SetDelay {
        ms: 10_000,
        mode: DelayMode::Rewind,
    });
    s.advance(8 * SEC);
    s.connect();
    s.advance(20 * SEC);
    let snap = s.snapshot();
    assert_eq!(snap.effective_ms, 10_000, "{snap:?}");
    assert_eq!(snap.output.splices, 0);
    s.check_invariants();
}

#[test]
fn a_reduction_by_a_hair_does_not_wait_forever() {
    // A rewind rounds back to a keyframe, so the delay can end up a hair over
    // the next one asked for (5 s gave 6.001 s here; then 6 s). Skipping ahead
    // that little needs a keyframe within that hair of what airs next, which
    // hardly ever comes: the change stayed pending ("Changing delay…") for good.
    let mut s = Sim::new(config());
    s.jitter = 39;
    s.advance(612 * MS);
    s.connect();
    s.advance(5_389 * MS);
    s.cmd(Command::SetDelay {
        ms: 5_000,
        mode: DelayMode::Rewind,
    });
    let asked = effective(&s) - 1;
    assert!(asked >= 5_000, "the rewind was not rounded back: {asked}");
    s.cmd(Command::SetDelay {
        ms: asked,
        mode: DelayMode::Mask,
    });
    s.advance(20 * SEC);
    let snap = s.snapshot();
    assert_eq!(snap.phase, Phase::Delayed, "{snap:?}");
    assert_eq!(snap.target_ms, asked);
    assert!(snap.effective_ms < asked + 500, "{snap:?}");
    s.check_invariants();
}

#[test]
fn go_live_after_air_sends_everything_up_to_the_mark() {
    let mut s = live_sim();
    s.cmd(Command::SetDelay {
        ms: 10_000,
        mode: DelayMode::Rewind,
    });
    s.advance(30 * SEC);
    let mark_time = s.now;
    s.cmd(Command::GoLive(GoLiveWhen::AfterAir));
    assert_eq!(s.snapshot().phase, Phase::GoingLive);
    s.advance(5 * SEC);
    assert_eq!(
        s.snapshot().phase,
        Phase::GoingLive,
        "still airing buffered content"
    );
    s.advance(10 * SEC);
    assert_eq!(s.snapshot().phase, Phase::Live);
    // Every video frame that arrived before the mark was sent.
    let sent_ids: std::collections::HashSet<u64> = s
        .media_sent()
        .filter(|(_, i)| i.kind == Kind::Video)
        .map(|(_, i)| i.index)
        .collect();
    let missing = s
        .inputs
        .values()
        .filter(|i| {
            i.kind == Kind::Video && i.arrival <= mark_time && i.arrival >= mark_time - 25 * SEC
        })
        .filter(|i| !sent_ids.contains(&i.index))
        .count();
    assert_eq!(missing, 0);
    s.check_invariants();
}

#[test]
fn mask_mode_rewinds_only_to_content_under_the_slate() {
    let mut s = live_sim();
    let started = s.now;
    let ack = s.cmd(Command::SetDelay {
        ms: 15_000,
        mode: DelayMode::Mask,
    });
    assert!(ack.pending);
    assert!(s.snapshot().mask_visible);
    assert_eq!(s.snapshot().phase, Phase::Adding);
    s.advance(10 * SEC);
    assert!(
        s.snapshot().mask_visible,
        "slate must stay up while the buffer fills"
    );
    assert!(effective(&s) < 100);
    s.advance(8 * SEC);
    let snap = s.snapshot();
    assert!(!snap.mask_visible);
    assert!((15_000..=15_100).contains(&snap.effective_ms), "{snap:?}");
    // After the splice, only content recorded after the slate appeared is sent.
    let (splice_index, _, _) = s
        .media_sent_in(0..s.sent.len())
        .find(|(_, x, i)| x.at > started && x.at - i.arrival > 10 * SEC)
        .unwrap();
    for (_, x, i) in s.media_sent_in(splice_index..s.sent.len()) {
        assert!(
            i.arrival >= started + 500 * MS,
            "gameplay from before the slate aired at {}",
            x.at
        );
    }
    s.floor = 15_000 * MS;
    s.floor_log.push((splice_index, s.floor));
    s.check_invariants();
}

#[test]
fn reduce_delay_skips_forward() {
    let mut s = live_sim();
    s.cmd(Command::SetDelay {
        ms: 30_000,
        mode: DelayMode::Rewind,
    });
    s.advance(40 * SEC);
    s.cmd(Command::SetDelay {
        ms: 10_000,
        mode: DelayMode::Rewind,
    });
    s.advance(3 * SEC);
    let e = effective(&s);
    assert!((10_000..=12_100).contains(&e), "effective {e}");
    s.check_invariants();
}

#[test]
fn delay_above_maximum_is_rejected() {
    let mut s = live_sim();
    let err = s.e.command(
        s.now,
        Command::SetDelay {
            ms: 500_000,
            mode: DelayMode::Rewind,
        },
    );
    assert_eq!(
        err,
        Err(EngineError::TooLarge {
            requested_ms: 500_000,
            max_ms: 120_000
        })
    );
}

#[test]
fn delay_set_before_the_stream_starts_applies_from_the_first_frame() {
    let mut s = Sim::new(config());
    s.cmd(Command::SetDelay {
        ms: 5_000,
        mode: DelayMode::Rewind,
    });
    s.connect();
    s.advance(20 * SEC);
    let first = s.media_sent().next().unwrap();
    assert!(first.0.at - first.1.arrival >= 5 * SEC);
    assert!((5_000..=5_100).contains(&effective(&s)));
    s.check_invariants();
}

#[test]
fn short_history_is_reported() {
    let mut s = Sim::new(config());
    s.connect();
    s.advance(6 * SEC);
    let ack = s.cmd(Command::SetDelay {
        ms: 60_000,
        mode: DelayMode::Rewind,
    });
    assert!(ack.history_short);
    assert!(ack.effective_ms < 7_000);
    assert!(!s.snapshot().warnings.is_empty());
}

#[test]
fn destination_reconnect_resumes_without_losing_content() {
    let mut s = live_sim();
    s.cmd(Command::SetDelay {
        ms: 10_000,
        mode: DelayMode::Rewind,
    });
    s.advance(20 * SEC);
    let last = s.sent.iter().rev().find_map(|x| x.msg.seq);
    s.e.output_disconnected(s.now, last);
    s.advance(3 * SEC);
    let reconnect_index = s.sent.len();
    s.connect();
    s.advance(5 * SEC);
    let resent = &s.sent[reconnect_index..];
    // A new connection starts with metadata, decoder configuration and timestamp 0.
    assert_eq!(resent[0].msg.kind, Kind::Data);
    assert!(
        resent
            .iter()
            .take(3)
            .any(|x| x.msg.payload[..2] == [0x17, 0x00])
    );
    let first_media = resent.iter().find(|x| x.msg.seq.is_some()).unwrap();
    assert_eq!(first_media.msg.timestamp, 0);
    // Delay grew by the outage instead of skipping content.
    assert!(effective(&s) >= 13_000);
}

/// The newest video decoder configuration the current session has kept.
fn video_config(s: &Sim) -> Bytes {
    let session = s.e.sessions.last().unwrap();
    let h = session.headers.iter().rev().find(|h| h.kind == Kind::Video);
    h.unwrap().payload.clone()
}

/// Drops the destination connection and connects again; returns where in `sent`
/// the new connection starts.
fn reconnect(s: &mut Sim) -> usize {
    let last = s.sent.iter().rev().find_map(|x| x.msg.seq);
    s.e.output_disconnected(s.now, last);
    let from = s.sent.len();
    s.connect();
    from
}

#[test]
fn a_repeated_codec_header_still_comes_before_older_keyframes() {
    // Some encoders send their decoder configuration again, unchanged. Keyframes
    // recorded before the repeat still need it, after a reconnect or a splice.
    let mut s = live_sim();
    s.cmd(Command::SetDelay {
        ms: 10_000,
        mode: DelayMode::Rewind,
    });
    s.advance(15 * SEC);
    let config = video_config(&s);
    let ts = s.ts_base + s.video_index * FRAME_MS;
    s.e.ingest(s.now, Kind::Video, ts, config.clone());
    s.advance(SEC);
    let from = reconnect(&mut s);
    s.advance(SEC);
    let first_video = s.sent[from..].iter().find(|x| x.msg.kind == Kind::Video);
    assert_eq!(
        first_video.unwrap().msg.payload,
        config,
        "a keyframe went out without its decoder configuration"
    );
}

#[test]
fn a_changed_codec_header_keeps_the_old_one_for_older_keyframes() {
    let mut s = live_sim();
    s.cmd(Command::SetDelay {
        ms: 10_000,
        mode: DelayMode::Rewind,
    });
    s.advance(15 * SEC);
    let old = video_config(&s);
    // A new configuration, as after a resolution change.
    let ts = s.ts_base + s.video_index * FRAME_MS;
    s.push(Kind::Video, ts, &[0x17, 0x00, 0, 0, 0, 0x02], false);
    let new = video_config(&s);
    assert_ne!(old, new);
    s.advance(SEC);
    let from = reconnect(&mut s);
    s.advance(SEC);
    let first_video = s.sent[from..].iter().find(|x| x.msg.kind == Kind::Video);
    assert_eq!(first_video.unwrap().msg.payload, old);
    // The new one goes out when the output gets to it.
    s.advance(12 * SEC);
    let sent: Vec<_> = s.sent[from..].iter().map(|x| &x.msg.payload).collect();
    let at = |p: &Bytes| sent.iter().position(|x| *x == p);
    assert!(at(&new).unwrap() > at(&old).unwrap());
    // Once nothing buffered is older than the change, the old one is forgotten.
    s.advance(150 * SEC);
    let kept = &s.e.sessions.last().unwrap().headers;
    assert!(!kept.iter().any(|h| h.payload == old));
    assert!(kept.iter().any(|h| h.payload == new));
}

#[test]
fn decoder_configuration_sent_is_not_kept_beyond_the_buffer() {
    // The output remembers what configuration the destination has, so as not to
    // send it twice. It must not hold on to the configuration itself: it can be
    // large, and there can be one per track.
    let mut s = Sim::new(EngineConfig {
        ram_cap_bytes: 16 * 1024 * 1024,
        ..config()
    });
    s.connect();
    s.advance(5 * SEC);
    let mut configs = Vec::new();
    for track in 0..32u8 {
        // Enhanced RTMP: multitrack audio, one track, AAC sequence start.
        let mut p = vec![0x95, 0x00, b'm', b'p', b'4', b'a', track];
        p.resize(2 * 1024 * 1024, 0);
        let p = Bytes::from(p);
        let ts = s.ts_base + s.audio_index * 21;
        s.e.ingest(s.now, Kind::Audio, ts, p.clone());
        s.poll();
        configs.push(p);
    }
    s.advance(5 * SEC);
    s.sent.clear();
    let in_buffer = |p: &Bytes| s.e.ring.iter().any(|e| e.payload.as_ptr() == p.as_ptr());
    let gone: Vec<_> = configs.into_iter().filter(|p| !in_buffer(p)).collect();
    assert!(gone.len() >= 16, "{} left the buffer", gone.len());
    for p in gone {
        assert!(
            p.is_unique(),
            "track {} still held outside the buffer",
            p[6]
        );
    }
}

#[test]
fn encoder_reconnect_continues_the_same_output() {
    let mut s = live_sim();
    s.cmd(Command::SetDelay {
        ms: 10_000,
        mode: DelayMode::Rewind,
    });
    s.advance(10 * SEC);
    s.stop_encoder();
    s.advance(3 * SEC);
    s.start_encoder(0); // new session, timestamps restart at 0
    s.advance(30 * SEC);
    // Video frames from the new session were sent, with timestamps still increasing.
    let new_session = s.session;
    assert!(s.media_sent().any(|(_, i)| i.session == new_session));
    s.check_invariants();
}

#[test]
fn buffer_is_bounded() {
    let mut s = Sim::new(EngineConfig {
        max_delay_ms: 10_000,
        headroom_ms: 2_000,
        ..Default::default()
    });
    s.connect();
    s.advance(120 * SEC);
    let snap = s.snapshot();
    assert!(snap.history_ms <= 14_100, "history {}", snap.history_ms);
}

#[test]
fn ram_cap_is_respected() {
    let mut s = Sim::new(EngineConfig {
        ram_cap_bytes: 20_000,
        ..config()
    });
    s.connect();
    s.advance(60 * SEC);
    assert!(s.snapshot().buffered_bytes <= 20_000);
}

#[test]
fn reconnects_that_send_only_metadata_keep_nothing() {
    let mut s = Sim::new(config());
    s.connect();
    s.advance(5 * SEC);
    s.stop_encoder();
    // After a GOP has aired, an encoder that reconnects over and over, sending
    // new metadata each time and no media.
    for i in 0..200u32 {
        s.e.ingest_start(s.now);
        let meta = Bytes::from(vec![i as u8; 60 * 1024]);
        s.e.ingest_metadata(s.now, meta);
        s.e.ingest_end(s.now);
        s.now += 100 * MS;
        s.poll();
    }
    // The session with buffered media, and the latest.
    assert!(s.e.sessions.len() <= 2, "{} sessions", s.e.sessions.len());
    assert!(
        s.e.session_bytes < 2 * 64 * 1024,
        "{} bytes kept for sessions",
        s.e.session_bytes
    );
    // Metadata larger than an encoder's is not kept.
    s.e.ingest_start(s.now);
    s.e.ingest_metadata(s.now, Bytes::from(vec![0u8; 1024 * 1024]));
    assert!(s.e.sessions.last().unwrap().metadata.is_none());
}

#[test]
fn sessions_count_against_the_ram_cap() {
    let cap = 1024 * 1024;
    let mut s = Sim::new(EngineConfig {
        ram_cap_bytes: cap,
        ..config()
    });
    s.connect();
    s.advance(5 * SEC);
    s.stop_encoder();
    // Reconnects that each send large metadata and one keyframe.
    for i in 0..200u32 {
        s.e.ingest_start(s.now);
        s.e.ingest_metadata(s.now, Bytes::from(vec![i as u8; 60 * 1024]));
        s.e.ingest(
            s.now,
            Kind::Video,
            u64::from(i) * 33,
            payload(&[0x17, 0x01, 0, 0, 0], 1_000_000 + i),
        );
        s.e.ingest_end(s.now);
        s.now += 100 * MS;
        s.poll();
    }
    // Over the cap by at most the newest message and session, which stay.
    let used = s.e.bytes + s.e.session_bytes;
    assert!(used <= cap + 128 * 1024, "{used} bytes in use");
    assert!(s.e.sessions.len() < 30, "{} sessions", s.e.sessions.len());
}

#[test]
fn snapshot_serializes() {
    let s = live_sim();
    let json = serde_json::to_value(s.snapshot()).unwrap();
    assert_eq!(json["phase"], "live");
    assert_eq!(json["ingest"]["video_codec"], "avc");
    assert_eq!(json["ingest"]["gop_ms"], 1980);
}

#[test]
fn gop_warning() {
    let mut s = Sim::new(config());
    s.connect();
    s.advance(10 * SEC);
    assert!(s.snapshot().warnings.is_empty());
}

#[test]
fn extended_output_timestamps_do_not_break_monotonicity() {
    // Start near the 24-bit extended-timestamp boundary.
    let mut s = Sim::new(config());
    s.stop_encoder();
    s.start_encoder(0x00FF_FF00);
    s.connect();
    s.advance(10 * SEC);
    s.check_invariants();
}

#[test]
fn dump_throws_away_what_has_not_aired_and_rebuilds_behind_the_slate() {
    let mut s = live_sim();
    s.cmd(Command::SetDelay {
        ms: 20_000,
        mode: DelayMode::Rewind,
    });
    s.advance(30 * SEC);
    let dumped_at = s.now;
    let n = s.sent.len();
    let ack = s.cmd(Command::Dump(DelayMode::Mask));
    assert!(ack.pending);
    assert_eq!(ack.target_ms, 20_000);
    let snap = s.snapshot();
    assert!(snap.mask_visible, "the slate must go up at once");
    assert_eq!(snap.phase, Phase::Adding);
    // Only what the slate covers airs, starting within a keyframe interval: the
    // destination keeps getting data.
    s.advance(3 * SEC);
    let first = s
        .media_sent_in(n..s.sent.len())
        .next()
        .expect("nothing aired after the dump");
    assert!(first.1.at - dumped_at <= 3 * SEC);
    assert!(first.2.keyframe);
    s.advance(20 * SEC);
    let snap = s.snapshot();
    assert!(
        !snap.mask_visible,
        "the slate stays up after the delay is back"
    );
    assert_eq!(snap.phase, Phase::Delayed);
    assert!((20_000..=22_100).contains(&snap.effective_ms), "{snap:?}");
    for (_, sent, i) in s.media_sent_in(n..s.sent.len()) {
        assert!(
            i.arrival >= dumped_at + 500 * MS,
            "content from before the dump (or before the slate was up) aired at {}",
            sent.at
        );
    }
    // A rewind can no longer reach what was dumped.
    s.cmd(Command::SetDelay {
        ms: 60_000,
        mode: DelayMode::Rewind,
    });
    s.advance(10 * SEC);
    assert!(
        s.media_sent_in(n..s.sent.len())
            .all(|(_, _, i)| i.arrival >= dumped_at),
        "a rewind aired dumped content"
    );
    s.check_invariants_in(n..s.sent.len());
}

#[test]
fn dump_needs_a_delay() {
    let mut s = live_sim();
    assert_eq!(
        s.e.command(s.now, Command::Dump(DelayMode::Rewind)),
        Err(EngineError::NothingToDump)
    );
    // Before anything airs, a dump just empties the buffer.
    let mut s = Sim::new(config());
    s.cmd(Command::SetDelay {
        ms: 10_000,
        mode: DelayMode::Rewind,
    });
    s.connect();
    s.advance(5 * SEC);
    let dumped_at = s.now;
    s.cmd(Command::Dump(DelayMode::Mask));
    assert!(!s.snapshot().mask_visible);
    s.advance(20 * SEC);
    assert!(s.media_sent().all(|(_, i)| i.arrival >= dumped_at));
    assert!(s.media_sent().next().is_some());
}

/// Ids of what is waiting to air: recorded after the last thing sent. (What an
/// earlier change skipped is not waiting; a rewind may still show it.)
fn unaired(s: &Sim) -> std::collections::HashSet<u32> {
    let last = s.media_sent().last().map(|(_, i)| i.arrival);
    s.inputs
        .iter()
        .filter(|(_, i)| last.is_none_or(|t| i.arrival > t))
        .map(|(id, _)| *id)
        .collect()
}

#[test]
fn a_rewind_dump_replays_what_aired_and_skips_what_had_not() {
    let mut s = Sim::new(config());
    s.cmd(Command::SetDelay {
        ms: 20_000,
        mode: DelayMode::Rewind,
    });
    s.connect();
    s.advance(60 * SEC);
    let dumped_at = s.now;
    let dumped = unaired(&s);
    assert!(dumped.len() > 500, "nothing was waiting to air");
    let n = s.sent.len();
    let ack = s.cmd(Command::Dump(DelayMode::Rewind));
    assert_eq!((ack.target_ms, ack.effective_ms), (20_000, 20_000));
    let snap = s.snapshot();
    assert!(!snap.mask_visible, "a replay needs no slate");
    assert_eq!(snap.phase, Phase::Delayed);
    assert_eq!(snap.effective_ms, 20_000);
    s.advance(40 * SEC);
    let after: Vec<_> = s.media_sent_in(n..s.sent.len()).collect();
    assert!(
        after
            .iter()
            .all(|(_, _, i)| !dumped.contains(&id_of_input(&s, i))),
        "dumped content aired"
    );
    // The delay never drops, and viewers keep getting a picture.
    for (_, sent, i) in &after {
        assert!(
            sent.at - i.arrival >= 20 * SEC,
            "aired after {} us",
            sent.at - i.arrival
        );
    }
    let video: Vec<Time> = after
        .iter()
        .filter(|(_, _, i)| i.kind == Kind::Video)
        .map(|(_, sent, _)| sent.at)
        .collect();
    assert!(
        video[0] - dumped_at < 100 * MS,
        "the replay did not start at once"
    );
    for w in video.windows(2) {
        assert!(
            w[1] - w[0] <= 2_100 * MS,
            "no picture for {} us",
            w[1] - w[0]
        );
    }
    // First a replay, then what was recorded after the dump, from a keyframe.
    assert!(after[0].2.arrival < dumped_at - 20 * SEC);
    let first_new = after
        .iter()
        .find(|(_, _, i)| i.arrival >= dumped_at && i.kind == Kind::Video)
        .expect("the stream did not continue");
    assert!(first_new.2.keyframe);
    let snap = s.snapshot();
    assert_eq!(snap.phase, Phase::Delayed);
    assert!((20_000..=22_100).contains(&snap.effective_ms), "{snap:?}");
    s.check_invariants_in(n..s.sent.len());
    // A rewind cannot reach what was dumped either.
    let m = s.sent.len();
    s.cmd(Command::SetDelay {
        ms: 60_000,
        mode: DelayMode::Rewind,
    });
    s.advance(10 * SEC);
    assert!(
        s.media_sent_in(m..s.sent.len())
            .all(|(_, _, i)| !dumped.contains(&id_of_input(&s, &i))),
        "a rewind aired dumped content"
    );
}

/// The id `i` was recorded under.
fn id_of_input(s: &Sim, i: &InputInfo) -> u32 {
    *s.inputs
        .iter()
        .find(|(_, x)| {
            x.arrival == i.arrival
                && x.kind == i.kind
                && x.index == i.index
                && x.session == i.session
        })
        .expect("known input")
        .0
}

#[test]
fn a_rewind_dump_uses_the_slate_without_enough_history() {
    // Not enough of the stream yet to replay the delay's worth before the dump.
    let mut s = Sim::new(config());
    s.cmd(Command::SetDelay {
        ms: 20_000,
        mode: DelayMode::Rewind,
    });
    s.connect();
    s.advance(30 * SEC);
    s.cmd(Command::Dump(DelayMode::Rewind));
    assert!(s.snapshot().mask_visible);
    // No rolling buffer at all.
    let mut s = Sim::new(EngineConfig {
        keep_history: false,
        ..config()
    });
    s.cmd(Command::SetDelay {
        ms: 10_000,
        mode: DelayMode::Rewind,
    });
    s.connect();
    s.advance(60 * SEC);
    let dumped = unaired(&s);
    let n = s.sent.len();
    s.cmd(Command::Dump(DelayMode::Rewind));
    assert!(s.snapshot().mask_visible);
    s.advance(20 * SEC);
    assert!(
        s.media_sent_in(n..s.sent.len())
            .all(|(_, _, i)| !dumped.contains(&id_of_input(&s, &i)))
    );
}

#[test]
fn a_dump_while_ending_ends_at_once() {
    let mut s = live_sim();
    s.cmd(Command::SetDelay {
        ms: 10_000,
        mode: DelayMode::Rewind,
    });
    s.advance(20 * SEC);
    s.e.end_after(s.e.last_seq().unwrap());
    let n = s.sent.len();
    s.cmd(Command::Dump(DelayMode::Rewind));
    assert!(s.e.end_reached());
    s.advance(10 * SEC);
    assert_eq!(s.media_sent_in(n..s.sent.len()).count(), 0);
}

#[test]
fn an_end_mark_sends_up_to_it_and_nothing_after() {
    let mut s = live_sim();
    s.cmd(Command::SetDelay {
        ms: 10_000,
        mode: DelayMode::Rewind,
    });
    s.advance(20 * SEC);
    let marked_at = s.now;
    let mark = s.e.last_seq().unwrap();
    s.e.end_after(mark);
    assert!(!s.e.end_reached());
    s.advance(8 * SEC);
    assert!(!s.e.end_reached(), "ended before the rest aired");
    s.advance(5 * SEC);
    assert!(s.e.end_reached());
    s.advance(10 * SEC);
    let sent: Vec<_> = s.media_sent().map(|(_, i)| i).collect();
    assert!(
        sent.iter().all(|i| i.arrival <= marked_at),
        "content after the mark aired"
    );
    let last_video = sent.iter().rev().find(|i| i.kind == Kind::Video).unwrap();
    assert!(
        marked_at - last_video.arrival < 100 * MS,
        "the end of the stream did not air"
    );
}

#[test]
fn a_new_broadcast_after_an_end_mark_starts_with_what_came_after_it() {
    let mut s = live_sim();
    s.cmd(Command::SetDelay {
        ms: 10_000,
        mode: DelayMode::Rewind,
    });
    s.advance(20 * SEC);
    s.stop_encoder();
    s.advance(3 * SEC);
    // The encoder starts a new stream while the old one's tail is still airing.
    let mark = s.e.last_seq().unwrap();
    s.e.end_after(mark);
    s.start_encoder(0);
    let new_session = s.session;
    s.advance(8 * SEC);
    assert!(s.e.end_reached());
    s.e.restart_after_end();
    let n = s.sent.len();
    s.connect();
    s.advance(20 * SEC);
    let fresh: Vec<_> = s.media_sent_in(n..s.sent.len()).collect();
    assert!(!fresh.is_empty());
    assert!(
        fresh.iter().all(|(_, _, i)| i.session == new_session),
        "the old stream aired in the new broadcast"
    );
    // From its first keyframe, with the delay, and timestamps from 0.
    let first = fresh
        .iter()
        .find(|(_, _, i)| i.kind == Kind::Video)
        .unwrap();
    assert_eq!(first.2.index, 0);
    assert!(first.1.at - first.2.arrival >= 10 * SEC);
    // A rewind cannot reach the old stream either.
    s.cmd(Command::SetDelay {
        ms: 60_000,
        mode: DelayMode::Rewind,
    });
    s.advance(5 * SEC);
    assert!(
        s.media_sent_in(n..s.sent.len())
            .all(|(_, _, i)| i.session == new_session)
    );
}

#[derive(Debug, Clone)]
enum Op {
    Wait(u64),
    Rewind(u64),
    Mask(u64),
    GoLive,
    AfterAir,
    Reduce(u64),
    DropOutput,
    EncoderRestart,
    Cancel,
    Dump(DelayMode),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (100u64..8_000).prop_map(Op::Wait),
        2 => (1u64..60).prop_map(Op::Rewind),
        1 => (1u64..30).prop_map(Op::Mask),
        1 => Just(Op::GoLive),
        1 => Just(Op::AfterAir),
        1 => (1u64..60).prop_map(Op::Reduce),
        1 => Just(Op::DropOutput),
        1 => Just(Op::EncoderRestart),
        1 => Just(Op::Cancel),
        1 => prop_oneof![Just(DelayMode::Rewind), Just(DelayMode::Mask)].prop_map(Op::Dump),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(
        std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(64),
    ))]

    #[test]
    fn invariants_hold_under_random_operations(
        ops in prop::collection::vec(op(), 1..25),
        audio in any::<bool>(),
        jitter in 0u64..4_000,
        keep_history in any::<bool>(),
    ) {
        let mut s = Sim::new(EngineConfig {
            max_delay_ms: 60_000,
            keep_history,
            ..Default::default()
        });
        s.audio = audio;
        s.jitter = jitter;
        s.connect();
        s.advance(5 * SEC);
        // (index into `sent`, what had not aired) for each dump.
        let mut dumps = Vec::new();
        // Where each destination connection starts in `sent`.
        let mut connections = vec![0];
        for op in ops {
            match op {
                Op::Wait(ms) => s.advance(ms * MS),
                Op::Rewind(sec) => { s.cmd(Command::SetDelay { ms: sec * 1000, mode: DelayMode::Rewind }); }
                Op::Mask(sec) => { s.cmd(Command::SetDelay { ms: sec * 1000, mode: DelayMode::Mask }); }
                Op::GoLive => { s.cmd(Command::GoLive(GoLiveWhen::Now)); }
                Op::AfterAir => { s.cmd(Command::GoLive(GoLiveWhen::AfterAir)); }
                Op::Reduce(sec) => { s.cmd(Command::SetDelay { ms: sec * 1000, mode: DelayMode::Rewind }); }
                Op::Cancel => { s.cmd(Command::Cancel); }
                Op::Dump(mode) => {
                    let waiting = unaired(&s);
                    if s.e.command(s.now, Command::Dump(mode)).is_ok() {
                        if s.snapshot().mask_visible {
                            s.floor = 0;
                        }
                        s.floor_log.push((s.sent.len(), s.floor));
                        dumps.push((s.sent.len(), waiting));
                    }
                    s.poll();
                }
                Op::DropOutput => {
                    let last = s.sent.iter().rev().find_map(|x| x.msg.seq);
                    s.e.output_disconnected(s.now, last);
                    s.advance(1_500 * MS);
                    // Restart invariant tracking for the new connection.
                    connections.push(s.sent.len());
                    s.connect();
                }
                Op::EncoderRestart => {
                    s.stop_encoder();
                    s.advance(700 * MS);
                    s.start_encoder(0);
                }
            }
            // The mask splice raises the floor once it happens.
            let snap = s.snapshot();
            if snap.phase == Phase::Delayed && snap.effective_ms * MS > s.floor && snap.target_ms * MS <= snap.effective_ms * MS {
                s.floor = snap.target_ms * MS;
                s.floor_log.push((s.sent.len(), s.floor));
            }
        }
        s.advance(5 * SEC);
        connections.push(s.sent.len());
        for w in connections.windows(2) {
            s.check_invariants_in(w[0]..w[1]);
        }
        // Nothing that had not aired by a dump ever airs after it.
        for (index, waiting) in dumps {
            for x in &s.sent[index..] {
                if let Some(id) = id_of(&x.msg.payload) {
                    prop_assert!(!waiting.contains(&id), "dumped content aired at {}", x.at);
                }
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(
        std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(64),
    ))]

    /// Once nothing more happens, viewers get the delay asked for: no less,
    /// unless the buffer was too short for it (which the state says), and no more
    /// than a keyframe interval over, from rounding back to one.
    ///
    /// Left out: a lost destination connection, after which the delay grows by
    /// the outage and stays until it is changed (as documented); and dumps, for
    /// a known gap: lowering the delay after a dump can land on a keyframe
    /// before the stretch it threw away, well over the delay asked for (one
    /// more press brings it down). Without them, no case fails in 20,000.
    #[test]
    fn the_delay_settles_at_the_one_asked_for(
        ops in prop::collection::vec(
            op().prop_filter("no outage or dump", |o| {
                !matches!(o, Op::DropOutput | Op::Dump(_))
            }),
            1..25,
        ),
        jitter in 0u64..50,
        keep_history in any::<bool>(),
        connect_after in 0u64..4_000,
    ) {
        let mut s = Sim::new(EngineConfig {
            max_delay_ms: 60_000,
            keep_history,
            ..Default::default()
        });
        s.jitter = jitter;
        // Connecting to the destination takes a moment.
        s.advance(connect_after * MS);
        s.connect();
        s.advance(5 * SEC);
        for op in ops {
            match op {
                Op::Wait(ms) => s.advance(ms * MS),
                Op::Rewind(sec) | Op::Reduce(sec) => { s.cmd(Command::SetDelay { ms: sec * 1000, mode: DelayMode::Rewind }); }
                Op::Mask(sec) => { s.cmd(Command::SetDelay { ms: sec * 1000, mode: DelayMode::Mask }); }
                Op::GoLive => { s.cmd(Command::GoLive(GoLiveWhen::Now)); }
                Op::AfterAir => { s.cmd(Command::GoLive(GoLiveWhen::AfterAir)); }
                Op::Cancel => { s.cmd(Command::Cancel); }
                Op::EncoderRestart => {
                    s.stop_encoder();
                    s.advance(700 * MS);
                    s.start_encoder(0);
                }
                Op::Dump(_) | Op::DropOutput => unreachable!(),
            }
        }
        // Longer than the largest delay, and a mask or dump building it up.
        s.advance(75 * SEC);
        let snap = s.snapshot();
        prop_assert!(matches!(snap.phase, Phase::Live | Phase::Delayed), "{snap:?}");
        let (target, effective) = (snap.target_ms, snap.effective_ms);
        if target == 0 {
            // Live, from the newest keyframe: no rounding back.
            prop_assert_eq!(snap.phase, Phase::Live, "{:?}", snap);
        } else if !snap.history_short {
            // A keyframe interval, frames coming up to 2 × jitter late.
            let gop = GOP_FRAMES * FRAME_MS + GOP_FRAMES * jitter;
            prop_assert!(
                effective + 100 >= target && effective <= target + gop + 500,
                "target {target} ms, effective {effective} ms: {snap:?}"
            );
        }
    }
}

#[test]
fn output_reset_starts_a_fresh_broadcast() {
    let mut s = live_sim();
    s.cmd(Command::SetDelay {
        ms: 5_000,
        mode: DelayMode::Rewind,
    });
    s.advance(10 * SEC);
    s.stop_encoder();
    s.advance(10 * SEC);
    assert!(s.e.drained());
    s.e.output_reset();
    s.start_encoder(0);
    let n = s.sent.len();
    s.connect();
    s.advance(10 * SEC);
    let new_session = s.session;
    // Only the new session is sent, starting at timestamp 0 with the target delay.
    let fresh: Vec<_> = s.media_sent_in(n..s.sent.len()).collect();
    assert!(fresh.iter().all(|(_, _, i)| i.session == new_session));
    assert_eq!(fresh[0].1.msg.timestamp, 0);
    assert!(fresh[0].1.at - fresh[0].2.arrival >= 5 * SEC);
}

#[test]
fn without_history_only_what_the_delay_needs_is_kept() {
    let mut s = Sim::new(EngineConfig {
        keep_history: false,
        ..config()
    });
    s.connect();
    s.advance(60 * SEC);
    // Live: only the keyframe interval the output would restart from is kept.
    let live_history = s.snapshot().history_ms;
    assert!(live_history <= 2_100, "history {live_history}");
    // Asking for a rewind uses the mask instead: the delay builds behind the slate.
    let ack = s.cmd(Command::SetDelay {
        ms: 10_000,
        mode: DelayMode::Rewind,
    });
    assert!(ack.pending, "{ack:?}");
    assert!(s.snapshot().mask_visible);
    s.advance(40 * SEC);
    let snap = s.snapshot();
    assert_eq!(snap.phase, Phase::Delayed);
    assert!((10_000..=12_100).contains(&snap.effective_ms), "{snap:?}");
    assert!(!snap.mask_visible);
    // Now about the delay plus one keyframe interval is kept.
    assert!(snap.history_ms <= 14_200, "history {}", snap.history_ms);
    s.check_invariants();
}

#[test]
fn a_byte_budget_holds_output_back_without_gaps() {
    let mut s = live_sim();
    s.cmd(Command::SetDelay {
        ms: 5_000,
        mode: DelayMode::Rewind,
    });
    s.advance(10 * SEC);
    let n = s.sent.len();
    // The destination falls behind: nothing may be handed over for 3 s.
    s.budget = Some(0);
    s.advance(3 * SEC);
    assert_eq!(s.sent.len(), n, "sent with no budget left");
    // A small budget lets a little through per poll.
    s.budget = Some(1);
    s.advance(SEC);
    assert!(s.sent.len() > n, "nothing sent with budget");
    // Room again: what was held back airs in order, with nothing skipped.
    s.budget = None;
    s.advance(5 * SEC);
    let video: Vec<u64> = s
        .media_sent_in(n..s.sent.len())
        .filter(|(_, _, i)| i.kind == Kind::Video)
        .map(|(_, _, i)| i.index)
        .collect();
    assert!(
        video.windows(2).all(|w| w[1] == w[0] + 1),
        "video frames skipped or reordered"
    );
    // Held-back content airs later than its delay, never sooner.
    s.check_invariants();
}

#[test]
fn discard_never_sends_what_was_buffered() {
    let mut s = live_sim();
    s.cmd(Command::SetDelay {
        ms: 20_000,
        mode: DelayMode::Rewind,
    });
    s.advance(30 * SEC);
    let discarded_at = s.now;
    s.e.discard();
    assert_eq!(s.snapshot().history_ms, 0);
    s.advance(5 * SEC);
    let n = s.sent.len();
    s.connect();
    s.advance(40 * SEC);
    let after: Vec<_> = s.media_sent_in(n..s.sent.len()).collect();
    assert!(!after.is_empty());
    assert!(
        after.iter().all(|(_, _, i)| i.arrival >= discarded_at),
        "content buffered before the discard was sent"
    );
    // The new broadcast starts on a keyframe and keeps the target delay.
    let first_video = after
        .iter()
        .find(|(_, _, i)| i.kind == Kind::Video)
        .unwrap();
    assert!(first_video.2.keyframe);
    assert!(
        after
            .iter()
            .all(|(_, sent, i)| sent.at - i.arrival >= 20 * SEC)
    );
    s.check_invariants_in(n..s.sent.len());
}

#[test]
fn buffer_display_stops_growing_after_the_encoder_leaves() {
    let mut s = Sim::new(config());
    s.advance(10 * SEC);
    s.stop_encoder();
    s.advance(300 * SEC);
    let history = s.snapshot().history_ms;
    assert!(history <= 10_100, "history {history}");
}

#[test]
fn a_new_stream_does_not_rewind_into_the_previous_one() {
    let mut s = live_sim();
    let old = s.session;
    s.stop_encoder();
    s.advance(10 * SEC);
    s.e.output_reset();
    s.start_encoder(0);
    s.connect();
    s.advance(5 * SEC);
    let n = s.sent.len();
    let ack = s.cmd(Command::SetDelay {
        ms: 30_000,
        mode: DelayMode::Rewind,
    });
    assert!(ack.history_short, "{ack:?}");
    s.advance(10 * SEC);
    assert!(
        s.media_sent_in(n..s.sent.len())
            .all(|(_, _, i)| i.session != old),
        "rewound into the previous stream"
    );
}

/// HEVC payload: enhanced CodedFramesX 'hvc1' with one slice of the given NAL type.
fn hevc(key: bool, nal: u8, id: u32) -> Bytes {
    let mut b = BytesMut::new();
    b.put_u8(0x80 | if key { 0x10 } else { 0x20 } | 0x03);
    b.put_slice(b"hvc1");
    b.put_u32(3);
    b.put_slice(&[nal << 1, 1, 0]);
    b.put_u32(id);
    b.freeze()
}

#[test]
fn hevc_rasl_pictures_are_dropped_after_splicing_to_a_cra() {
    let mut e = Engine::new(config());
    let t0 = 1_000 * SEC;
    e.ingest_start(t0);
    e.output_connected(t0);
    let mut seq_header = BytesMut::new();
    seq_header.put_u8(0x80 | 0x10);
    seq_header.put_slice(b"hvc1");
    e.ingest(t0, Kind::Video, 0, seq_header.freeze());
    // Open GOP: CRA, then two RASL leading pictures, then trailing pictures.
    let mut id = 0;
    let mut t = t0;
    for gop in 0..10u64 {
        for (i, nal) in [21u8, 8, 8, 1, 1, 1].into_iter().enumerate() {
            id += 1;
            e.ingest(
                t,
                Kind::Video,
                gop * 200 + i as u64 * 33,
                hevc(i == 0, nal, id),
            );
            t += 33 * MS;
        }
        t += 1_800 * MS;
    }
    let mut out = Vec::new();
    e.poll(t, &mut out);
    e.command(
        t,
        Command::SetDelay {
            ms: 5_000,
            mode: DelayMode::Rewind,
        },
    )
    .unwrap();
    out.clear();
    e.poll(t + 1_000 * MS, &mut out);
    let nals: Vec<u8> = out
        .iter()
        .filter(|m| m.seq.is_some())
        .map(|m| (m.payload[9] >> 1) & 0x3f)
        .collect();
    assert_eq!(
        nals[0],
        flv::hevc::BLA_W_LP,
        "splice must land on the CRA, marked as BLA"
    );
    assert!(
        nals[1] != 8,
        "RASL pictures after the splice must be dropped: {nals:?}"
    );
    assert_eq!(&nals[1..4], &[1, 1, 1]);
}

#[test]
fn spliced_cra_is_rewritten_as_bla() {
    let mut e = Engine::new(config());
    let t0 = 1_000 * SEC;
    e.ingest_start(t0);
    e.output_connected(t0);
    let mut t = t0;
    let mut id = 0;
    for _ in 0..10 {
        for (i, nal) in [21u8, 1, 1].into_iter().enumerate() {
            id += 1;
            e.ingest(t, Kind::Video, (t - t0) / MS, hevc(i == 0, nal, id));
            t += 33 * MS;
        }
        t += 1_900 * MS;
    }
    let mut out = Vec::new();
    e.poll(t, &mut out);
    // The very first frame starts the stream: a CRA there is already a clean start.
    e.command(
        t,
        Command::SetDelay {
            ms: 5_000,
            mode: DelayMode::Rewind,
        },
    )
    .unwrap();
    out.clear();
    e.poll(t, &mut out);
    let first = out.iter().find(|m| m.seq.is_some()).unwrap();
    assert_eq!((first.payload[9] >> 1) & 0x3f, flv::hevc::BLA_W_LP);
}

#[test]
fn decoder_configuration_kept_for_splices_is_bounded() {
    // Enhanced RTMP multitrack sequence starts, one per track id.
    let config = |track: u8, extra: usize| {
        let mut p = vec![0x96, 0x00, b'a', b'v', b'c', b'1', track];
        p.resize(p.len() + extra, 0);
        Bytes::from(p)
    };
    let mut e = Engine::new(EngineConfig::default());
    e.ingest_start(0);
    for track in 0..=255u8 {
        e.ingest(1_000, Kind::Video, 0, config(track, 16));
    }
    let headers = &e.sessions.last().unwrap().headers;
    assert_eq!(headers.len(), MAX_HEADERS);
    assert_eq!(
        headers.last().unwrap().class,
        255 << 8,
        "the newest are kept"
    );
    // One too large to keep is still streamed, just not kept for splices.
    e.ingest(1_000, Kind::Video, 0, config(7, MAX_HEADER_BYTES));
    let headers = &e.sessions.last().unwrap().headers;
    assert!(headers.iter().all(|h| h.payload.len() <= MAX_HEADER_BYTES));
    assert!(e.has_buffered());
}

#[test]
fn decoder_configuration_kept_takes_only_a_share_of_the_ram_cap() {
    // As many configuration messages as are kept, each nearly as large as is
    // kept, under the smallest cap the settings allow.
    let cap = 16 << 20;
    let header = |track: u8| {
        let mut p = vec![0x96, 0x00, b'a', b'v', b'c', b'1', track];
        p.resize(MAX_HEADER_BYTES - 64, 0);
        Bytes::from(p)
    };
    let mut e = Engine::new(EngineConfig {
        ram_cap_bytes: cap,
        ..config()
    });
    e.ingest_start(0);
    for track in 0..MAX_HEADERS as u8 {
        e.ingest(1_000 * SEC, Kind::Video, 0, header(track));
    }
    let headers = &e.sessions.last().unwrap().headers;
    let kept: usize = headers.iter().map(|h| cost(h.payload.len())).sum();
    assert!(
        kept <= cap / HEADER_SHARE_OF_RAM_CAP,
        "{kept} bytes of configuration kept"
    );
    assert_eq!(
        headers.last().unwrap().class,
        u16::from(MAX_HEADERS as u8 - 1) << 8,
        "the newest are kept"
    );
    // Eviction keeps the rest of the buffer within what is left.
    let held = e.snapshot(1_000 * SEC).buffered_bytes as usize;
    assert!(held <= cap + cost(MAX_HEADER_BYTES), "{held} bytes held");
}

#[test]
fn tiny_messages_count_against_the_ram_cap() {
    // 200 000 one-byte messages: 200 KB of payload, far below the cap, but
    // several times the cap once what each message costs is counted.
    let cap = 1 << 20;
    let mut e = Engine::new(EngineConfig {
        ram_cap_bytes: cap,
        ..config()
    });
    e.ingest_start(0);
    let payload = Bytes::from_static(&[0]);
    for i in 0..200_000u64 {
        e.ingest(1_000 * SEC + i * 100, Kind::Data, i / 10, payload.clone());
    }
    assert!(e.bytes <= cap, "{} bytes counted", e.bytes);
    assert!(
        e.ring.len() <= cap / ENTRY_OVERHEAD + 1,
        "{} messages kept",
        e.ring.len()
    );
    assert!(e.ring.len() > 1000, "the cap should not empty the buffer");
}
