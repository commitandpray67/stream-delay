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
        self.wake = self.e.poll(self.now, &mut out);
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
                let key = self.video_index % GOP_FRAMES == 0;
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
    ) {
        let mut s = Sim::new(EngineConfig { max_delay_ms: 60_000, ..Default::default() });
        s.audio = audio;
        s.jitter = jitter;
        s.connect();
        s.advance(5 * SEC);
        for op in ops {
            match op {
                Op::Wait(ms) => s.advance(ms * MS),
                Op::Rewind(sec) => { s.cmd(Command::SetDelay { ms: sec * 1000, mode: DelayMode::Rewind }); }
                Op::Mask(sec) => { s.cmd(Command::SetDelay { ms: sec * 1000, mode: DelayMode::Mask }); }
                Op::GoLive => { s.cmd(Command::GoLive(GoLiveWhen::Now)); }
                Op::AfterAir => { s.cmd(Command::GoLive(GoLiveWhen::AfterAir)); }
                Op::Reduce(sec) => { s.cmd(Command::SetDelay { ms: sec * 1000, mode: DelayMode::Rewind }); }
                Op::Cancel => { s.cmd(Command::Cancel); }
                Op::DropOutput => {
                    let last = s.sent.iter().rev().find_map(|x| x.msg.seq);
                    s.e.output_disconnected(s.now, last);
                    s.advance(1_500 * MS);
                    // Restart invariant tracking for the new connection.
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
        check_split_by_connection(&s);
    }
}

/// Invariants are per destination connection (timestamps restart at 0 on reconnect),
/// so split the log wherever the output timestamp resets.
fn check_split_by_connection(s: &Sim) {
    let mut start = 0;
    for (i, x) in s.sent.iter().enumerate() {
        if i > start && x.msg.kind == Kind::Data && x.msg.seq.is_none() && x.msg.timestamp == 0 {
            s.check_invariants_in(start..i);
            start = i;
        }
    }
    s.check_invariants_in(start..s.sent.len());
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
